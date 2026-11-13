//! B4 T15 — J1 `jobOverview` (`GET /api/jobs/{job}/overview`) and J2 `jobAnalytics`
//! (`GET /api/jobs/{job}/analytics?days=7|30`): KPI windows, release windows by `agent_case.release_id`, Production
//! figures without the Test env's traffic, CSAT from `case_csat` (prod, answered, in the window). Real Postgres 18, the
//! real router, a job created from the contract template; cases, usage and ratings are seeded as the runtime writes
//! them (owner pool, RLS off).
use axum::http::StatusCode;
use serde_json::Value;
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, send, super_admin};

struct Fx {
    e: TestEnv,
    f: ContractJob,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    Fx { e, f }
}

async fn get(fx: &Fx, tail: &str) -> (StatusCode, Value) {
    let uri = format!("/api/jobs/{}{tail}", fx.f.job.job_key);
    let mut q = common::get(&uri, Some(&super_admin()));
    q.headers_mut().insert(
        "x-active-tenant-id",
        fx.f.tenant.to_string().parse().unwrap(),
    );
    send(&fx.e.app, q).await
}

async fn ok(fx: &Fx, tail: &str) -> Value {
    let (s, b) = get(fx, tail).await;
    assert_eq!(s, StatusCode::OK, "{tail}: {b}");
    b
}

/// One case as the runtime stores it.
struct Case<'a> {
    n: i64,
    env: &'a str,
    status: &'a str,
    reason: Option<&'a str>,
    days_ago: i32,
    release: Option<&'a str>,
    activity: &'a str,
}

impl Default for Case<'_> {
    fn default() -> Self {
        Self {
            n: 1,
            env: "prod",
            status: "resolved",
            reason: None,
            days_ago: 1,
            release: None,
            activity: "support:track",
        }
    }
}

/// `CS-<n>`: a test case runs on draft revision 1 (staff chat), a prod case on `release` (NULL = baseline).
async fn case(fx: &Fx, c: Case<'_>) {
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev, release_id, \
                                 status, escalation_reason, activity, started_at, duration_ms) \
         VALUES ($1, $2, 'CS-' || $3::int8, 'conv-' || $3::int8, \
                 CASE WHEN $4::text = 'test' THEN 'staff' ELSE 'web_chat' END, $4::text, \
                 CASE WHEN $4::text = 'test' THEN 1 END, $5, $6, $7, $8, \
                 now() - make_interval(days => $9), 1000 * $3::int8)",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(c.n)
    .bind(c.env)
    .bind(c.release)
    .bind(c.status)
    .bind(c.reason)
    .bind(c.activity)
    .bind(c.days_ago)
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

/// One metered call `days_ago` days back: `use_` by a customer (`customer_turn`) or a staff tester (`staff_test`).
async fn usage(fx: &Fx, use_: &str, staff: bool, tokens: i64, days_ago: i32) {
    sqlx::query(
        "INSERT INTO usage_event (tenant_id, job_id, event_key, occurred_at, source, use, actor, provider_id, model, \
                                  funding, tokens_in, tokens_out) \
         VALUES ($1, $2, $3, now() - make_interval(days => $4), $5, $6, $7, 'internal', 'local-llm', 'local_gpu', \
                 $8, 0)",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(Uuid::now_v7())
    .bind(days_ago)
    .bind(if staff { "staff_test" } else { "turn" })
    .bind(use_)
    .bind(if staff {
        "u-tester"
    } else {
        "callers:web_chat"
    })
    .bind(tokens)
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

/// A CSAT request of `CS-<n>` (same env and release as the case), answered `rating` `days_ago` days back.
async fn csat(fx: &Fx, n: i64, rating: Option<i16>, days_ago: i32) {
    sqlx::query(
        "INSERT INTO case_csat (tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev, \
                                requested_at, answered_at, rating, expires_at, state, asked_at) \
         SELECT tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev, \
                now() - make_interval(days => $4), \
                CASE WHEN $3::int2 IS NULL THEN NULL ELSE now() - make_interval(days => $4) END, $3::int2, \
                now() - make_interval(days => $4) + interval '24 hours', \
                CASE WHEN $3::int2 IS NULL THEN 'asked' ELSE 'answered' END, now() - make_interval(days => $4) \
           FROM agent_case WHERE job_id = $1 AND case_id = 'CS-' || $2::int8",
    )
    .bind(fx.f.job.job_id)
    .bind(n)
    .bind(rating)
    .bind(days_ago)
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

async fn exec(fx: &Fx, sql: &'static str) {
    sqlx::query(sql)
        .bind(fx.f.job.job_id)
        .execute(&fx.e.owner)
        .await
        .unwrap();
}

#[tokio::test]
async fn kpis_compare_two_windows() {
    let fx = setup().await;
    // this window (the last 30 UTC days): 4 cases, 2 contained, 1 escalated, 1 still open
    case(
        &fx,
        Case {
            n: 1,
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 2,
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 3,
            status: "handover",
            reason: Some("customer_asked"),
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 4,
            status: "open",
            ..Case::default()
        },
    )
    .await;
    // the 30 days before: 2 cases, 1 contained, 1 escalated (resolved by staff)
    case(
        &fx,
        Case {
            n: 5,
            days_ago: 40,
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 6,
            days_ago: 40,
            reason: Some("tool_failed"),
            ..Case::default()
        },
    )
    .await;
    // before both windows: never counted
    case(
        &fx,
        Case {
            n: 7,
            days_ago: 70,
            ..Case::default()
        },
    )
    .await;
    for _ in 0..3 {
        usage(&fx, "qualify", false, 10, 1).await;
    }
    usage(&fx, "reply", false, 10, 1).await; // not a turn
    usage(&fx, "qualify", false, 10, 40).await;
    usage(&fx, "qualify", false, 10, 70).await;

    let b = ok(&fx, "/overview").await;
    let k = &b["kpis"];
    assert_eq!(k["turns_30d"], 3, "{b}");
    assert_eq!(k["turns_prev"], 1, "{b}");
    assert_eq!(k["containment"], 0.5, "{b}");
    assert_eq!(k["containment_prev"], 0.5, "{b}");
    assert_eq!(k["escalation"], 0.25, "{b}");
    assert_eq!(k["escalation_prev"], 0.5, "{b}");
    assert_eq!(k["open_cases"], 2, "{b}");
    assert_eq!(b["pulse"]["open_by_status"]["handover"], 1, "{b}");
    assert_eq!(b["pulse"]["open_by_status"]["open"], 1, "{b}");
    assert_eq!(b["job"], fx.f.job.job_key.as_str());
    assert!(b["current_release"].is_null(), "never released: {b}");
    // a north star that is not containment counts the window's cases
    exec(
        &fx,
        "UPDATE jobs SET north_star = 'Orders answered' WHERE id = $1",
    )
    .await;
    let b = ok(&fx, "/overview").await;
    assert_eq!(b["kpis"]["north_star"]["unit"], "n", "{b}");
    assert_eq!(b["kpis"]["north_star"]["value"], 4, "{b}");
    exec(
        &fx,
        "UPDATE jobs SET north_star = 'Containment rate' WHERE id = $1",
    )
    .await;
    let b = ok(&fx, "/overview").await;
    assert_eq!(b["kpis"]["north_star"]["unit"], "pct", "{b}");
    assert_eq!(b["kpis"]["north_star"]["value"], 0.5, "{b}");

    // an unchecked unknown outcome is listed by case id until it is checked
    sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, case_id, operation, route, instance_id, env, state, \
                                     started_at, finished_at, initiator) \
         VALUES ($1, $2, $3, 'CS-4', 'refund.create', 'instance', 'demo-retail-stub', 'prod', 'unknown_outcome', \
                 now(), now(), 'runner@t15')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(format!("opc_{}", Uuid::now_v7()))
    .execute(&fx.e.owner)
    .await
    .unwrap();
    let b = ok(&fx, "/overview").await;
    let case_href = format!("/admin/jobs/{}/cases/CS-4", fx.f.job.job_key);
    let hrefs: Vec<&str> = b["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["href"].as_str())
        .collect();
    assert!(hrefs.contains(&case_href.as_str()), "{b}");
    let item = b["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["href"] == case_href.as_str())
        .unwrap();
    assert_eq!(item["severity"], "blocking", "{item}");
    assert_eq!(item["owner_role"], "operator", "{item}");
    // one call of one operation, failing: the pulse counts it and attention names it
    assert_eq!(b["pulse"]["op_calls_30d"], 1, "{b}");
    assert_eq!(b["pulse"]["failing_ops"][0]["op"], "refund.create", "{b}");
    exec(
        &fx,
        "INSERT INTO operation_call_check (tenant_id, job_id, call_id, verdict, by_sub) \
         SELECT tenant_id, job_id, call_id, 'landed', 'u-ops' FROM operation_call WHERE job_id = $1",
    )
    .await;
    let b = ok(&fx, "/overview").await;
    assert!(
        !b["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["href"] == case_href.as_str()),
        "a checked outcome leaves attention: {b}"
    );
}

#[tokio::test]
async fn release_window_splits_cases_by_release_id() {
    let fx = setup().await;
    let (r1, r2) = ("cfg-2026-10-01.1", "cfg-2026-10-15.1");
    for (id, state, days, hash) in [(r1, "applied_earlier", 20, 'a'), (r2, "current", 5, 'c')] {
        sqlx::query(
            r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
                   provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
                   snapshot_kinds)
               VALUES ($1, $2, $3, $4, $4, '{}', '{}', '[]', '{}', '{}', '{}', $5, 'u-rm',
                   now() - make_interval(days => $6 + 1), 'u-rm', now() - make_interval(days => $6), '{}')"#,
        )
        .bind(fx.f.tenant)
        .bind(fx.f.job.job_id)
        .bind(id)
        .bind(format!("sha256:{}", hash.to_string().repeat(64)))
        .bind(state)
        .bind(days)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(fx.f.job.job_id)
        .bind(r2)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    sqlx::query(
        r#"INSERT INTO release_bundled_draft (tenant_id, job_id, release_id, key, ledger_id, kind, ref, area, page,
               label, at, by_sub)
           VALUES ($1, $2, $3, 'rule:RET-01', 1, 'rule_values', '{"id": "RET-01"}', 'rules', '/rules',
               'Returns window 30 → 45 days', now(), 'u-rm')"#,
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(r2)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    // r1: half of `support:track` contained; r2: all of it
    case(
        &fx,
        Case {
            n: 1,
            release: Some(r1),
            days_ago: 15,
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 2,
            release: Some(r1),
            days_ago: 15,
            status: "handover",
            reason: Some("customer_asked"),
            ..Case::default()
        },
    )
    .await;
    for n in [3, 4] {
        case(
            &fx,
            Case {
                n,
                release: Some(r2),
                ..Case::default()
            },
        )
        .await;
    }
    case(
        &fx,
        Case {
            n: 5,
            release: Some(r2),
            activity: "support:returns",
            ..Case::default()
        },
    )
    .await;
    // neither the baseline nor the Test env belongs to a release window
    case(
        &fx,
        Case {
            n: 6,
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 7,
            env: "test",
            ..Case::default()
        },
    )
    .await;
    csat(&fx, 3, Some(5), 0).await;

    let b = ok(&fx, "/analytics?days=30").await;
    let w = b["release_windows"].as_array().unwrap();
    assert_eq!(w.len(), 2, "{b}");
    let (cur, early) = (&w[0], &w[1]);
    assert_eq!(cur["release_id"], r2, "newest cut first: {b}");
    assert_eq!(cur["status"], "current");
    assert_eq!(
        (&cur["cases"], &cur["contained"], &cur["escalated"]),
        (&Value::from(3), &Value::from(3), &Value::from(0)),
        "{cur}"
    );
    assert!(cur["until"].is_null(), "{cur}");
    assert_eq!(cur["csat"], 5.0, "{cur}");
    assert_eq!(cur["csat_responses"], 1, "{cur}");
    assert_eq!(cur["changes"][0], "Returns window 30 → 45 days", "{cur}");
    assert_eq!(cur["moves"][0]["activity"], "support:track", "{cur}");
    assert_eq!(cur["moves"][0]["from"], 0.5, "{cur}");
    assert_eq!(cur["moves"][0]["to"], 1.0, "{cur}");
    assert_eq!(early["release_id"], r1);
    assert_eq!(early["status"], "earlier");
    assert_eq!(
        (&early["cases"], &early["contained"], &early["escalated"]),
        (&Value::from(2), &Value::from(1), &Value::from(1)),
        "{early}"
    );
    assert_eq!(
        early["until"], cur["applied_at"],
        "r1 ran until r2 went live: {b}"
    );
    assert!(early["csat"].is_null(), "{early}");
    assert_eq!(early["moves"].as_array().unwrap().len(), 0, "{early}");

    let o = ok(&fx, "/overview").await;
    assert_eq!(o["current_release"]["id"], r2, "{o}");
}

#[tokio::test]
async fn test_traffic_excluded_from_prod_figures() {
    let fx = setup().await;
    case(
        &fx,
        Case {
            n: 1,
            days_ago: 0,
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 2,
            env: "test",
            days_ago: 0,
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 3,
            env: "test",
            days_ago: 0,
            status: "handover",
            reason: Some("staff_decision"),
            ..Case::default()
        },
    )
    .await;
    for _ in 0..2 {
        usage(&fx, "qualify", false, 100, 0).await;
    }
    for _ in 0..5 {
        usage(&fx, "qualify", true, 1000, 0).await;
    }

    let o = ok(&fx, "/overview").await;
    assert_eq!(o["kpis"]["turns_30d"], 2, "{o}");
    assert_eq!(o["kpis"]["containment"], 1.0, "{o}");
    assert_eq!(o["kpis"]["escalation"], 0.0, "{o}");
    assert_eq!(o["kpis"]["open_cases"], 0, "{o}");
    assert_eq!(o["pulse"]["open_prod"], 0, "{o}");
    assert_eq!(o["pulse"]["open_test"], 1, "{o}");

    let a = ok(&fx, "/analytics?days=7").await;
    let days = a["days"].as_array().unwrap();
    assert_eq!(days.len(), 7, "{a}");
    let sum = |k: &str| days.iter().map(|d| d[k].as_i64().unwrap()).sum::<i64>();
    assert_eq!(
        (sum("cases"), sum("turns"), sum("escalated")),
        (1, 2, 0),
        "{a}"
    );
    assert_eq!(a["tokens"], 200, "{a}");
    assert_eq!(a["funding"]["local_gpu"], 200, "{a}");
    assert!(
        a["escalation_reasons"].as_array().unwrap().is_empty(),
        "{a}"
    );
    let rev: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(fx.f.job.job_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    let t = &a["test_env"];
    assert_eq!(t["draft_rev"], rev, "{a}");
    assert_eq!(
        (&t["chats"], &t["contained"], &t["escalated"]),
        (&Value::from(2), &Value::from(1), &Value::from(1)),
        "{a}"
    );
}

#[tokio::test]
async fn analytics_csat_excludes_test_env() {
    let fx = setup().await;
    for n in [1, 2] {
        case(
            &fx,
            Case {
                n,
                days_ago: 0,
                ..Case::default()
            },
        )
        .await;
    }
    case(
        &fx,
        Case {
            n: 3,
            env: "test",
            days_ago: 0,
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 4,
            days_ago: 10,
            ..Case::default()
        },
    )
    .await;
    case(
        &fx,
        Case {
            n: 5,
            days_ago: 0,
            ..Case::default()
        },
    )
    .await;
    csat(&fx, 1, Some(4), 0).await;
    csat(&fx, 2, Some(2), 0).await;
    csat(&fx, 3, Some(5), 0).await; // test env: excluded
    csat(&fx, 4, Some(3), 10).await; // the 7 days before
    csat(&fx, 5, None, 0).await; // asked, not answered

    let a = ok(&fx, "/analytics?days=7").await;
    assert_eq!(a["csat"]["score"], 3.0, "{a}");
    assert_eq!(a["csat"]["responses"], 2, "{a}");
    assert_eq!(a["csat"]["previous"], 3.0, "{a}");

    let (s, b) = get(&fx, "/analytics?days=90").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_query", "{b}");
}

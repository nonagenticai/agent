//! Console redesign T7 — the cross-agent inbox (`GET /api/inbox[?job=]`, crate::routes::inbox over
//! crate::readmodel::job_attention and `home::attention`): ranking, tenant and job scope, tenant isolation, and that
//! the counts of one job equal the `urgent` / `review` of its status. Real Postgres 18, no bus (the attention here is
//! database facts only), jobs created through the API.
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{self, TestEnv, person, send, super_admin};

struct Job {
    tenant: Uuid,
    key: String,
    id: Uuid,
}

/// A job of `tenant` created from `layer@version` on the platform route (a super_admin assigns templates of any
/// industry; the tenant route refuses a second industry, D123).
async fn create_job(e: &TestEnv, tenant: Uuid, layer: &str, version: i32) -> Job {
    let key = layer.rsplit('/').next().unwrap();
    let q = common::json(
        "POST",
        &format!("/api/tenants/{tenant}/jobs"),
        Some(&super_admin()),
        json!({"template": format!("{layer}@v{version}"), "key": key, "label": key}),
    );
    let (s, b) = send(&e.app, q).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let key = b["key"].as_str().unwrap().to_string();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    Job { tenant, key, id }
}

async fn new_tenant(e: &TestEnv, name: &str) -> Uuid {
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, &format!("{name} {t}")).await;
    common::licence_active(&e.owner, t).await;
    t
}

/// A handed-over case `n` of `job`, queued and unclaimed, whose first-response time went by two hours ago.
async fn breached_handover(e: &TestEnv, job: &Job, n: i64) {
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, status) \
         VALUES ($1, $2, 'CS-' || $3::int8, 'conv-' || $3::int8, 'web_chat', 'prod', 'handover')",
    )
    .bind(job.tenant)
    .bind(job.id)
    .bind(n)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_handover_items (tenant_id, job_id, id, case_id, conversation_id, env, reason, \
                                           state, first_response_due, resolve_due) \
         VALUES ($1, $2, 'HO-' || $3::int8, 'CS-' || $3::int8, 'conv-' || $3::int8, 'prod', \
                 'customer_asked', 'queued', now() - interval '2 hours', now() + interval '1 day')",
    )
    .bind(job.tenant)
    .bind(job.id)
    .bind(n)
    .execute(&e.owner)
    .await
    .unwrap();
}

/// An unchecked unknown outcome on case `n` (the case must exist).
async fn unknown_outcome(e: &TestEnv, job: &Job, n: i64) {
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, status) \
         VALUES ($1, $2, 'CS-' || $3::int8, 'conv-' || $3::int8, 'web_chat', 'prod', 'open')",
    )
    .bind(job.tenant)
    .bind(job.id)
    .bind(n)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, case_id, operation, route, instance_id, env, state, \
                                     started_at, finished_at, initiator) \
         VALUES ($1, $2, $3, 'CS-' || $4::int8, 'refund.create', 'instance', 'demo-retail-stub', 'prod', \
                 'unknown_outcome', now(), now(), 'runner@t7')",
    )
    .bind(job.tenant)
    .bind(job.id)
    .bind(format!("opc_{}", Uuid::now_v7()))
    .bind(n)
    .execute(&e.owner)
    .await
    .unwrap();
}

/// GET `uri` as a super_admin acting in `tenant`.
async fn get_in(e: &TestEnv, tenant: Uuid, uri: &str) -> (StatusCode, Value) {
    let mut q = common::get(uri, Some(&super_admin()));
    q.headers_mut()
        .insert("x-active-tenant-id", tenant.to_string().parse().unwrap());
    send(&e.app, q).await
}

async fn inbox_in(e: &TestEnv, tenant: Uuid, tail: &str) -> Value {
    let (s, b) = get_in(e, tenant, &format!("/api/inbox{tail}")).await;
    assert_eq!(s, StatusCode::OK, "{tail}: {b}");
    b
}

fn items(b: &Value) -> &Vec<Value> {
    b["items"].as_array().unwrap()
}

const CCAG: (&str, i32) = ("job/customer_care_agent", 4);

/// A second job template of the platform's own making (a super_admin assigns any template): a blank job layer
/// `job/billing_desk` with no spaces — the inbox only needs a second job of the tenant, not another industry.
async fn billing_template(e: &TestEnv) -> (String, i32) {
    crate::b4::it_desk_identity::own_template(e, "billing_desk", &[]).await
}

#[tokio::test]
async fn inbox_ranks_sla_breach_first() {
    let e = common::env().await;
    let t = new_tenant(&e, "Rank").await;
    let job = create_job(&e, t, CCAG.0, CCAG.1).await;
    unknown_outcome(&e, &job, 1).await;
    breached_handover(&e, &job, 2).await;
    let b = inbox_in(&e, t, &format!("?job={}", job.key)).await;
    let list = items(&b);
    assert_eq!(list[0]["kind"], "handover_sla", "{b}");
    assert_eq!(list[0]["severity"], "blocking", "{b}");
    assert_eq!(list[0]["owner_role"], "support_agent", "{b}");
    assert!(list[0]["due_at"].is_string(), "the SLA deadline: {b}");
    assert_eq!(
        list[1]["kind"], "unknown_outcome",
        "no due time ranks after one: {b}"
    );
    assert_eq!(list[1]["owner_role"], "operator", "{b}");
    // plain-language title; the raw text moved to `code`
    let (t, d) = (
        list[1]["title"].as_str().unwrap(),
        list[1]["detail"].as_str().unwrap(),
    );
    assert!(
        !t.contains("unknown_outcome") && !d.contains("unknown_outcome"),
        "{b}"
    );
    assert!(
        list[1]["code"]
            .as_str()
            .unwrap()
            .contains("unknown_outcome"),
        "{b}"
    );
    assert!(list[1]["due_at"].is_null(), "{b}");
    // blocking before action before info
    let rank = |s: &Value| match s.as_str() {
        Some("blocking") => 0,
        Some("action") => 1,
        _ => 2,
    };
    let ranks: Vec<i32> = list.iter().map(|i| rank(&i["severity"])).collect();
    assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{ranks:?}: {b}");
    // the unclaimed handover is an action for the support manager
    let unclaimed = list
        .iter()
        .find(|i| i["kind"] == "handover_unclaimed")
        .unwrap();
    assert_eq!(unclaimed["severity"], "action", "{unclaimed}");
    assert_eq!(unclaimed["owner_role"], "support_manager", "{unclaimed}");
    // every item carries the AttentionItem fields
    for i in list {
        for k in [
            "id",
            "kind",
            "severity",
            "title",
            "detail",
            "href",
            "action",
            "job",
            "at",
            "owner_role",
            "due_at",
            "code",
        ] {
            assert!(i.get(k).is_some(), "{k} missing: {i}");
        }
    }
}

#[tokio::test]
async fn inbox_tenant_scope_includes_every_job_and_tenant_items() {
    let e = common::env().await;
    let billing = billing_template(&e).await;
    let t = new_tenant(&e, "Scope").await;
    let a = create_job(&e, t, CCAG.0, CCAG.1).await;
    let b = create_job(&e, t, &billing.0, billing.1).await;
    breached_handover(&e, &a, 1).await;
    breached_handover(&e, &b, 1).await;
    let body = inbox_in(&e, t, "").await;
    let jobs: Vec<&str> = items(&body)
        .iter()
        .filter_map(|i| i["job"].as_str())
        .collect();
    assert!(jobs.contains(&a.key.as_str()), "{body}");
    assert!(jobs.contains(&b.key.as_str()), "{body}");
    assert!(
        items(&body)
            .iter()
            .any(|i| i["kind"] == "licence" && i["job"].is_null()),
        "a tenant item rides along (no tenant licence is issued in the fixture): {body}"
    );
    let c = &body["counts"];
    let n = |s: &str| items(&body).iter().filter(|i| i["severity"] == s).count();
    assert_eq!(c["blocking"], n("blocking"), "{body}");
    assert_eq!(c["action"], n("action"), "{body}");
    assert_eq!(c["info"], n("info"), "{body}");
    assert!(
        c["blocking"].as_i64().unwrap() >= 2,
        "one SLA breach per job: {body}"
    );
}

#[tokio::test]
async fn inbox_job_scope_excludes_other_jobs() {
    let e = common::env().await;
    let billing = billing_template(&e).await;
    let t = new_tenant(&e, "OneJob").await;
    let a = create_job(&e, t, CCAG.0, CCAG.1).await;
    let b = create_job(&e, t, &billing.0, billing.1).await;
    breached_handover(&e, &a, 1).await;
    breached_handover(&e, &b, 1).await;
    let body = inbox_in(&e, t, &format!("?job={}", a.key)).await;
    assert!(!items(&body).is_empty(), "{body}");
    for i in items(&body) {
        assert_eq!(i["job"], a.key.as_str(), "only this job: {i}");
    }
    let (s, err) = get_in(&e, t, "/api/inbox?job=no_such_job").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{err}");
    assert_eq!(err["error"]["code"], "job_not_found", "{err}");
}

#[tokio::test]
async fn inbox_hides_jobs_the_caller_cannot_see() {
    let e = common::env().await;
    let billing = billing_template(&e).await;
    let mine = new_tenant(&e, "Mine").await;
    let other = new_tenant(&e, "Other").await;
    let a = create_job(&e, mine, CCAG.0, CCAG.1).await;
    let b = create_job(&e, other, &billing.0, billing.1).await;
    breached_handover(&e, &a, 1).await;
    breached_handover(&e, &b, 1).await;
    common::insert_user(&e.owner, "u-obs", mine, "tenant_user").await;
    let (s, body) = send(
        &e.app,
        common::get("/api/inbox", Some(&person("u-obs", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let jobs: Vec<&str> = items(&body)
        .iter()
        .filter_map(|i| i["job"].as_str())
        .collect();
    assert!(jobs.contains(&a.key.as_str()), "{body}");
    assert!(
        !jobs.contains(&b.key.as_str()),
        "another tenant's job never shows: {body}"
    );
    // nor can the job of another tenant be asked for by key
    let (s, err) = send(
        &e.app,
        common::get(
            &format!("/api/inbox?job={}", b.key),
            Some(&person("u-obs", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{err}");
}

#[tokio::test]
async fn status_counts_equal_inbox_counts_for_job() {
    let e = common::env().await;
    let t = new_tenant(&e, "Counts").await;
    let job = create_job(&e, t, CCAG.0, CCAG.1).await;
    unknown_outcome(&e, &job, 1).await;
    breached_handover(&e, &job, 2).await;
    let inbox = inbox_in(&e, t, &format!("?job={}", job.key)).await;
    let (s, st) = get_in(&e, t, &format!("/api/jobs/{}/status", job.key)).await;
    assert_eq!(s, StatusCode::OK, "{st}");
    assert_eq!(st["urgent"], inbox["counts"]["blocking"], "{st} / {inbox}");
    assert_eq!(st["review"], inbox["counts"]["action"], "{st} / {inbox}");
    assert!(st["urgent"].as_i64().unwrap() >= 2, "{st}");
}

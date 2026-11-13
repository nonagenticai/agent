//! B1 T7: the append-only tenant audit log, the platform audit log and the audit helper contract (crate::audit).
//! Real Postgres 18 (one container for this binary).
use crate::common;

use std::path::Path;

use agent_api::audit::{
    self, Actor, Event, PlatformEvent, Status, record, record_alone, record_platform, vocab,
};
use agent_api::observability::with_request_id;
use agent_api::platform_db::{enter, leave};
use agent_api::tenancy::begin_scoped;
use common::{insert_tenant, sqlstate};
use uuid::Uuid;

async fn tenant_env() -> (common::TestEnv, Uuid) {
    let e = common::env_bare().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    (e, t)
}

fn admin() -> Actor {
    Actor::member("u-admin", false)
}

#[tokio::test]
async fn record_writes_actor_action_job_status_request_id() {
    let (e, t) = tenant_env().await;
    let id = with_request_id("req-abc-1".into(), async {
        let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
        let id = record(
            &mut tx,
            &Actor::member("u-super", true),
            Event::new("user.add", "user", "u-9")
                .job("faq_agent")
                .status(Status::Denied)
                .detail("email_domain", "b.eu"),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        id
    })
    .await;
    #[allow(clippy::type_complexity)]
    let row: (Uuid, String, String, bool, String, String, String, Option<String>, String, serde_json::Value, Option<String>) =
        sqlx::query_as(
            "SELECT tenant_id, actor_id, actor_type, platform_actor, action, resource_type, resource_id, job, \
                    status, details, request_id FROM audit_event WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(row.0, t, "the tenant is the transaction's GUC");
    assert_eq!(
        (row.1.as_str(), row.2.as_str(), row.3),
        ("u-super", "human", true)
    );
    assert_eq!(
        (row.4.as_str(), row.5.as_str(), row.6.as_str()),
        ("user.add", "user", "u-9")
    );
    assert_eq!(row.7.as_deref(), Some("faq_agent"));
    assert_eq!(row.8, "denied");
    assert_eq!(row.9, serde_json::json!({"email_domain": "b.eu"}));
    assert_eq!(row.10.as_deref(), Some("req-abc-1"));
}

#[tokio::test]
async fn record_without_tenant_guc_is_an_error() {
    let (e, _) = tenant_env().await;
    let mut tx = e.state.db.begin().await.unwrap();
    let err = record(&mut tx, &admin(), Event::new("user.add", "user", "u-9"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "internal", "{err}");
}

#[tokio::test]
async fn app_role_cannot_update_or_delete_audit() {
    let (e, t) = tenant_env().await;
    audit::record_alone(
        &e.state.db,
        t,
        &admin(),
        Event::new("user.add", "user", "u-9"),
    )
    .await
    .unwrap();
    for sql in [
        "UPDATE audit_event SET status = 'success'",
        "DELETE FROM audit_event",
        "TRUNCATE audit_event",
    ] {
        let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
        let err = sqlx::query(sql).execute(&mut *tx).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn unknown_action_is_refused() {
    let (e, t) = tenant_env().await;
    let err = record_alone(
        &e.state.db,
        t,
        &admin(),
        Event::new("user.teleport", "user", "u-9"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), "internal");
    // a platform action is not a tenant action
    let err = record_alone(
        &e.state.db,
        t,
        &admin(),
        Event::new("signup.approve", "signup", "s"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), "internal");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn details_with_token_or_email_is_refused() {
    let (e, t) = tenant_env().await;
    let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ1LTEifQ.c2lnbmF0dXJl";
    for ev in [
        Event::new("user.add", "user", "u-9").detail("token", "abc"),
        Event::new("user.add", "user", "u-9").detail("client_secret", "abc"),
        Event::new("user.add", "user", "u-9").detail("who", "a@b.eu"),
        Event::new("user.add", "user", "u-9").detail("credential", jwt),
    ] {
        let err = record_alone(&e.state.db, t, &admin(), ev)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "internal");
    }
    record_alone(
        &e.state.db,
        t,
        &admin(),
        Event::new("user.invite", "user", "u-9").detail("email_domain", "b.eu"),
    )
    .await
    .expect("the domain is allowed");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn record_alone_survives_a_rolled_back_write() {
    let (e, t) = tenant_env().await;
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    sqlx::query("INSERT INTO users (sub, tenant_id, updated_by) VALUES ('u-9', $1, 't')")
        .bind(t)
        .execute(&mut *tx)
        .await
        .unwrap();
    record(&mut tx, &admin(), Event::new("user.add", "user", "u-9"))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    record_alone(
        &e.state.db,
        t,
        &admin(),
        Event::new("user.add", "user", "u-9").failed("keycloak_error"),
    )
    .await
    .unwrap();
    let rows: Vec<String> = sqlx::query_scalar("SELECT status FROM audit_event")
        .fetch_all(&e.owner)
        .await
        .unwrap();
    assert_eq!(rows, ["failure"], "only the record_alone row survives");
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(users, 0);
}

#[tokio::test]
async fn platform_record_outside_platform_role_fails() {
    let (e, t) = tenant_env().await;
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let err = record_platform(
        &mut tx,
        &Actor::member("u-super", true),
        PlatformEvent::new("tenant.suspend", "Alpha").tenant(t),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), "internal");
    drop(tx);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM platform_audit")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn platform_record_inside_enter_ok() {
    let (e, t) = tenant_env().await;
    let sa = Actor::member("u-super", true);
    // the mixed transaction of spec 04 §9b: tenant row, then the platform row, one commit
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    record(
        &mut tx,
        &sa,
        Event::new("tenant.suspend", "tenant", t.to_string()),
    )
    .await
    .unwrap();
    enter(&mut tx).await.unwrap();
    record_platform(
        &mut tx,
        &sa,
        PlatformEvent::new("tenant.suspend", "Alpha")
            .tenant(t)
            .detail("reason_code", "billing"),
    )
    .await
    .unwrap();
    leave(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    let (actor_type, action, subject, status): (String, String, Option<Uuid>, String) =
        sqlx::query_as("SELECT actor_type, action, subject_tenant, status FROM platform_audit")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (
            actor_type.as_str(),
            action.as_str(),
            subject,
            status.as_str()
        ),
        ("user", "tenant.suspend", Some(t), "ok")
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
    // a platform action outside the vocabulary is refused even as agent_platform
    let err = audit::record_platform_alone(&e.state.db, &sa, PlatformEvent::new("job.create", "x"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "internal");
}

#[tokio::test]
async fn once_per_request_execute_rows_dedupe() {
    let (e, t) = tenant_env().await;
    let ids = with_request_id("req-exec-1".into(), async {
        let mut ids = Vec::new();
        for _ in 0..2 {
            let id = record_alone(
                &e.state.db,
                t,
                &Actor::agent("faq_router"),
                Event::new("execute", "operation", "op-1").once_per_request(),
            )
            .await
            .unwrap();
            ids.push(id);
        }
        ids
    })
    .await;
    assert_eq!(ids[0], ids[1], "the second record returns the first row");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'execute'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
    // another request is another row
    with_request_id("req-exec-2".into(), async {
        record_alone(
            &e.state.db,
            t,
            &Actor::agent("faq_router"),
            Event::new("execute", "operation", "op-1").once_per_request(),
        )
        .await
        .unwrap();
    })
    .await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'execute'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn tenant_a_cannot_read_tenant_b_audit() {
    let (e, a) = tenant_env().await;
    let b = Uuid::new_v4();
    insert_tenant(&e.owner, b, "Beta").await;
    record_alone(
        &e.state.db,
        a,
        &admin(),
        Event::new("user.add", "user", "u-a"),
    )
    .await
    .unwrap();
    record_alone(
        &e.state.db,
        b,
        &admin(),
        Event::new("user.add", "user", "u-b"),
    )
    .await
    .unwrap();
    let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
    let seen: Vec<String> = sqlx::query_scalar("SELECT resource_id FROM audit_event")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, ["u-a"]);
}

/// Every `Event::new("…")` / `PlatformEvent::new("…")` literal under `dir`, as (is_platform, action, file).
fn literals(dir: &Path, out: &mut Vec<(bool, String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            literals(&path, out);
            continue;
        }
        if path.extension().and_then(|x| x.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let needle = "Event::new(\"";
        let mut from = 0;
        while let Some(i) = text[from..].find(needle) {
            let at = from + i;
            let start = at + needle.len();
            let end = start + text[start..].find('"').unwrap();
            let platform = text[..at].ends_with("Platform");
            out.push((
                platform,
                text[start..end].to_string(),
                path.display().to_string(),
            ));
            from = end;
        }
    }
}

#[test]
fn vocab_is_sorted_unique_and_covers_every_literal() {
    assert!(
        vocab::sorted_unique(vocab::TENANT),
        "TENANT sorted + unique"
    );
    assert!(
        vocab::sorted_unique(vocab::PLATFORM),
        "PLATFORM sorted + unique"
    );
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    literals(&src, &mut found);
    let missing: Vec<_> = found
        .iter()
        .filter(|(platform, action, _)| {
            let list = if *platform {
                vocab::PLATFORM
            } else {
                vocab::TENANT
            };
            !list.contains(&action.as_str())
        })
        .collect();
    assert!(
        missing.is_empty(),
        "actions not in audit::vocab: {missing:?}"
    );
}

// ---- B1 T16: the audit read routes --------------------------------------------------------------------------------

use axum::http::StatusCode;
use common::{TestEnv, get, insert_user, person, send, super_admin};
use serde_json::{Value, json};

/// A tenant-log row written as the owner; its id.
#[allow(clippy::too_many_arguments)]
async fn row(
    e: &TestEnv,
    t: Uuid,
    actor: &str,
    actor_type: &str,
    action: &str,
    resource: &str,
    job: Option<&str>,
    status: &str,
    details: Value,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO audit_event (tenant_id, actor_id, actor_type, action, resource_type, resource_id, job, status, details) \
         VALUES ($1, $2, $3, $4, CASE WHEN $4 LIKE 'user.%' THEN 'user' ELSE 'thing' END, $5, $6, $7, $8) RETURNING id",
    )
    .bind(t)
    .bind(actor)
    .bind(actor_type)
    .bind(action)
    .bind(resource)
    .bind(job)
    .bind(status)
    .bind(details)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn audit_tenant() -> (TestEnv, Uuid) {
    let (e, t) = tenant_env().await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    (e, t)
}

async fn list(e: &TestEnv, query: &str) -> (StatusCode, Value) {
    send(
        &e.app,
        get(&format!("/api/audit{query}"), Some(&person("u-admin", &[]))),
    )
    .await
}

fn actions(b: &Value) -> Vec<String> {
    b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["action"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn filters_by_family_view_action_prefix_job_actor() {
    let (e, t) = audit_tenant().await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'faq_agent', 'FAQ', '[]', 't')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let ok = json!({});
    row(
        &e,
        t,
        "u-admin",
        "human",
        "user.invite",
        "u-new",
        None,
        "success",
        ok.clone(),
    )
    .await;
    row(
        &e,
        t,
        "u-admin",
        "human",
        "user.role",
        "u-new",
        None,
        "success",
        ok.clone(),
    )
    .await;
    row(
        &e,
        t,
        "u-super",
        "human",
        "job.create",
        "faq_agent",
        Some("faq_agent"),
        "success",
        ok.clone(),
    )
    .await;
    row(
        &e,
        t,
        "faq_agent",
        "ai_agent",
        "execute",
        "op-1",
        Some("faq_agent"),
        "success",
        ok.clone(),
    )
    .await;
    row(
        &e,
        t,
        "service:x",
        "system",
        "provision.failed",
        "p-1",
        None,
        "failure",
        ok.clone(),
    )
    .await;
    row(
        &e,
        t,
        "u-admin",
        "human",
        "misc.thing",
        "m-1",
        None,
        "success",
        ok.clone(),
    )
    .await;

    let check = |q: &'static str, want: &'static [&'static str]| (q, want);
    for (q, want) in [
        check("?family=access", &["user.role", "user.invite"]),
        check("?family=calls", &["execute"]),
        check("?family=platform", &["provision.failed"]),
        check("?family=other", &["misc.thing"]),
        check("?view=calls", &["execute"]),
        check(
            "?view=changes",
            &[
                "misc.thing",
                "provision.failed",
                "job.create",
                "user.role",
                "user.invite",
            ],
        ),
        check("?action=user", &["user.role", "user.invite"]),
        check("?action=user.role", &["user.role"]),
        check("?job=faq_agent", &["execute", "job.create"]),
        check(
            "?actor=u-admin",
            &["misc.thing", "user.role", "user.invite"],
        ),
        check("?actor_type=ai_agent", &["execute"]),
        check("?status=failure", &["provision.failed"]),
        check("?q=INVITE", &["user.invite"]),
    ] {
        let (s, b) = list(&e, q).await;
        assert_eq!(s, StatusCode::OK, "{q}: {b}");
        assert_eq!(actions(&b), want, "{q}");
    }
    for (q, code, status) in [
        (
            "?family=nope",
            "invalid_family",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "?view=both",
            "invalid_view",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("?job=nope", "job_not_found", StatusCode::NOT_FOUND),
        (
            "?limit=500",
            "invalid_limit",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (s, b) = list(&e, q).await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (status, Some(code)),
            "{q}: {b}"
        );
    }
    // read-time names
    let (_, b) = list(&e, "?action=user.invite").await;
    assert_eq!(b["items"][0]["actor_name"], "u-admin");
    assert_eq!(b["items"][0]["platform_actor"], false);
    assert!(b["items"][0]["id"].is_string());
}

#[tokio::test]
async fn outcome_resolved_by_later_success_same_resource() {
    let (e, t) = audit_tenant().await;
    let failed = row(
        &e,
        t,
        "service:x",
        "system",
        "provision.failed",
        "p-1",
        None,
        "failure",
        json!({}),
    )
    .await;
    let other = row(
        &e,
        t,
        "service:x",
        "system",
        "provision.failed",
        "p-2",
        None,
        "error",
        json!({}),
    )
    .await;
    let fix = row(
        &e,
        t,
        "u-admin",
        "human",
        "provision.retried",
        "p-1",
        None,
        "success",
        json!({}),
    )
    .await;
    let (s, b) = list(&e, "").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let of = |id: i64| {
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(id))
            .unwrap()
            .clone()
    };
    let resolved = of(failed);
    assert_eq!(resolved["outcome"]["state"], "resolved");
    assert_eq!(resolved["outcome"]["by_id"], fix.to_string());
    assert_eq!(resolved["outcome"]["by_action"], "provision.retried");
    assert_eq!(of(other)["outcome"]["state"], "open");
    assert!(
        of(fix).get("outcome").is_none(),
        "success rows have no outcome"
    );
}

#[tokio::test]
async fn execute_rows_open_only_with_alert() {
    let (e, t) = audit_tenant().await;
    let alert = row(
        &e,
        t,
        "faq_agent",
        "ai_agent",
        "execute",
        "op-1",
        None,
        "error",
        json!({"alert": true}),
    )
    .await;
    let quiet = row(
        &e,
        t,
        "faq_agent",
        "ai_agent",
        "execute",
        "op-1",
        None,
        "error",
        json!({}),
    )
    .await;
    row(
        &e,
        t,
        "faq_agent",
        "ai_agent",
        "execute",
        "op-1",
        None,
        "success",
        json!({}),
    )
    .await;
    let (_, b) = list(&e, "?view=calls").await;
    let items = b["items"].as_array().unwrap();
    let get_row = |id: i64| {
        items
            .iter()
            .find(|r| r["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(id))
            .unwrap()
            .clone()
    };
    assert_eq!(
        get_row(alert)["outcome"]["state"],
        "open",
        "a later successful call does not resolve an alerted one"
    );
    assert!(get_row(quiet).get("outcome").is_none());
}

#[tokio::test]
async fn summary_counts_whole_log() {
    let (e, t) = audit_tenant().await;
    row(
        &e,
        t,
        "u-admin",
        "human",
        "user.invite",
        "u-1",
        None,
        "success",
        json!({}),
    )
    .await;
    row(
        &e,
        t,
        "u-admin",
        "human",
        "user.role",
        "u-1",
        None,
        "success",
        json!({}),
    )
    .await;
    row(
        &e,
        t,
        "service:x",
        "system",
        "provision.failed",
        "p-1",
        None,
        "failure",
        json!({}),
    )
    .await;
    row(
        &e,
        t,
        "faq_agent",
        "ai_agent",
        "execute",
        "op",
        None,
        "success",
        json!({}),
    )
    .await;
    row(
        &e,
        t,
        "faq_agent",
        "ai_agent",
        "execute",
        "op",
        None,
        "denied",
        json!({}),
    )
    .await;
    row(
        &e,
        t,
        "faq_agent",
        "ai_agent",
        "execute",
        "op",
        None,
        "failure",
        json!({"alert": true}),
    )
    .await;
    let (s, b) = send(
        &e.app,
        get("/api/audit/summary", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["total"], 6);
    assert_eq!(b["changes"], 3);
    assert_eq!(
        b["calls"],
        json!({"total": 3, "success": 1, "denied": 1, "error": 1, "fallback": 0})
    );
    let open: Vec<&str> = b["open"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["action"].as_str().unwrap())
        .collect();
    assert_eq!(open, ["execute", "provision.failed"], "newest first");
    let fam = |f: &str| {
        b["families"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["family"] == f)
            .unwrap()
            .clone()
    };
    assert_eq!(fam("access")["count"], 2);
    assert_eq!(fam("platform")["count"], 1);
    assert_eq!(fam("platform")["open"], 1);
    assert_eq!(b["families"].as_array().unwrap().len(), 5);
    assert_eq!(b["actors"][0]["actor_id"], "faq_agent", "most active first");
    assert_eq!(b["actors"][0]["count"], 3);
}

#[tokio::test]
async fn member_sees_only_own_tenant() {
    let (e, a) = audit_tenant().await;
    let b_t = Uuid::new_v4();
    insert_tenant(&e.owner, b_t, "Beta").await;
    row(
        &e,
        a,
        "u-admin",
        "human",
        "user.role",
        "u-1",
        None,
        "success",
        json!({}),
    )
    .await;
    row(
        &e,
        b_t,
        "u-b",
        "human",
        "user.disable",
        "u-2",
        None,
        "success",
        json!({}),
    )
    .await;
    let (_, body) = list(&e, "").await;
    assert_eq!(actions(&body), ["user.role"]);
    let mut req = get("/api/audit", Some(&super_admin()));
    req.headers_mut()
        .insert("x-active-tenant-id", b_t.to_string().parse().unwrap());
    let (s, body) = send(&e.app, req).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    // the super_admin's switch into Beta is itself audited there
    assert!(actions(&body).contains(&"user.disable".to_string()));
    assert!(!actions(&body).contains(&"user.role".to_string()));
    let (s, _) = send(&e.app, get("/api/audit", Some(&person("u-nobody", &[])))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn platform_audit_needs_sa() {
    let (e, _t) = audit_tenant().await;
    for uri in ["/api/platform/audit", "/api/platform/audit/summary"] {
        let (s, b) = send(&e.app, get(uri, Some(&person("u-admin", &[])))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(b["error"]["code"], "super_admin_required");
        let (s, b) = send(&e.app, get(uri, Some(&super_admin()))).await;
        assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    }
    let (s, b) = send(
        &e.app,
        get("/api/platform/audit?family=nope", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "invalid_family");
}

async fn platform_row(e: &TestEnv, action: &str, label: &str, status: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO platform_audit (actor_id, actor_type, action, subject_label, status) \
         VALUES ('u-super', 'user', $1, $2, $3) RETURNING id",
    )
    .bind(action)
    .bind(label)
    .bind(status)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn platform_outcome_resolved_by_later_create_in_thread() {
    let (e, _t) = audit_tenant().await;
    let first = platform_row(&e, "tenant.create", "Northwind (first attempt)", "failed").await;
    let created = platform_row(&e, "tenant.create", "Northwind", "ok").await;
    let lonely = platform_row(&e, "license.issue_failed", "Southco", "failed").await;
    platform_row(&e, "signup.intake", "Eastco", "ok").await;
    let (s, b) = send(&e.app, get("/api/platform/audit", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let items = b["items"].as_array().unwrap();
    let of = |id: i64| {
        items
            .iter()
            .find(|r| r["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(id))
            .unwrap()
            .clone()
    };
    assert_eq!(of(first)["outcome"]["state"], "resolved");
    assert_eq!(of(first)["outcome"]["by_id"], created.to_string());
    assert_eq!(of(lonely)["outcome"]["state"], "open");
    assert!(of(created).get("outcome").is_none());
    let (_, b) = send(
        &e.app,
        get("/api/platform/audit?family=creation", Some(&super_admin())),
    )
    .await;
    assert_eq!(b["items"].as_array().unwrap().len(), 3);
    let (_, s) = send(
        &e.app,
        get("/api/platform/audit/summary", Some(&super_admin())),
    )
    .await;
    assert_eq!(s["total"], 4);
    assert_eq!(s["failed"], 2);
    assert_eq!(s["open"].as_array().unwrap().len(), 1);
    assert_eq!(s["open"][0]["id"], lonely.to_string());
    let creation = s["families"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["family"] == "creation")
        .unwrap()
        .clone();
    assert_eq!(
        creation,
        json!({"family": "creation", "count": 3, "failed": 2, "open": 1})
    );
    assert_eq!(
        s["actions"],
        json!(["license.issue_failed", "signup.intake", "tenant.create"])
    );
}

#[tokio::test]
async fn cursor_pages_without_gaps() {
    let (e, t) = audit_tenant().await;
    let mut ids = Vec::new();
    for i in 0..7 {
        ids.push(
            row(
                &e,
                t,
                "u-admin",
                "human",
                "user.role",
                &format!("u-{i}"),
                None,
                "success",
                json!({}),
            )
            .await,
        );
    }
    ids.reverse();
    let mut seen: Vec<i64> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let q = match &cursor {
            Some(c) => format!("?limit=3&before={c}"),
            None => "?limit=3".to_string(),
        };
        let (s, b) = list(&e, &q).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        for r in b["items"].as_array().unwrap() {
            seen.push(r["id"].as_str().unwrap().parse().unwrap());
        }
        match b["next"].as_str() {
            Some(n) => cursor = Some(n.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, ids, "every row once, newest first");
}

// ---- B1b T4 (F-04): platform audit subject-tenant and since filters ---------------------------------------------

/// A platform row about `tenant` at `at` (as the owner).
async fn platform_row_at(
    e: &TestEnv,
    action: &str,
    tenant: Option<Uuid>,
    at: &str,
    status: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO platform_audit (actor_id, actor_type, action, subject_tenant, subject_label, status, at) \
         VALUES ('u-super', 'user', $1, $2, 'Subject', $3, $4::timestamptz) RETURNING id",
    )
    .bind(action)
    .bind(tenant)
    .bind(status)
    .bind(at)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn platform_ids(e: &TestEnv, query: &str) -> Vec<i64> {
    let (s, b) = send(
        &e.app,
        get(&format!("/api/platform/audit{query}"), Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{query}: {b}");
    b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().parse().unwrap())
        .collect()
}

#[tokio::test]
async fn platform_audit_filters_by_subject_tenant() {
    let (e, a) = audit_tenant().await;
    let b = Uuid::new_v4();
    insert_tenant(&e.owner, b, "Beta").await;
    let a1 = platform_row_at(&e, "tenant.create", Some(a), "2026-10-01T10:00:00Z", "ok").await;
    let b1 = platform_row_at(&e, "tenant.create", Some(b), "2026-10-02T10:00:00Z", "ok").await;
    let a2 = platform_row_at(&e, "tenant.suspend", Some(a), "2026-10-03T10:00:00Z", "ok").await;
    let none = platform_row_at(&e, "signup.intake", None, "2026-10-04T10:00:00Z", "ok").await;
    assert_eq!(platform_ids(&e, &format!("?tenant={a}")).await, [a2, a1]);
    assert_eq!(platform_ids(&e, &format!("?tenant={b}")).await, [b1]);
    assert_eq!(
        platform_ids(&e, "").await,
        [none, a2, b1, a1],
        "no filter: all"
    );
    assert_eq!(
        platform_ids(&e, "?tenant=").await.len(),
        4,
        "empty = not given"
    );
    // an unknown tenant id is valid and matches nothing
    assert!(
        platform_ids(&e, &format!("?tenant={}", Uuid::new_v4()))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn platform_audit_since_is_inclusive_utc() {
    let (e, a) = audit_tenant().await;
    let before = platform_row_at(&e, "tenant.create", Some(a), "2026-10-19T23:59:59Z", "ok").await;
    let midnight =
        platform_row_at(&e, "tenant.suspend", Some(a), "2026-10-20T00:00:00Z", "ok").await;
    let later = platform_row_at(&e, "tenant.resume", Some(a), "2026-10-20T12:00:00Z", "ok").await;
    assert_eq!(
        platform_ids(&e, "?since=2026-10-20").await,
        [later, midnight],
        "a date is its UTC midnight, inclusive"
    );
    // an RFC 3339 timestamp with an offset is compared in UTC: 01:00+02:00 = 2026-10-19T23:00Z
    assert_eq!(
        platform_ids(&e, "?since=2026-10-20T01:00:00%2B02:00").await,
        [later, midnight, before]
    );
    assert_eq!(
        platform_ids(&e, "?since=2026-10-20T12:00:00Z").await,
        [later],
        "inclusive at the instant"
    );
}

#[tokio::test]
async fn platform_audit_tenant_and_since_combine_with_family_and_cursor() {
    let (e, a) = audit_tenant().await;
    let b = Uuid::new_v4();
    insert_tenant(&e.owner, b, "Beta").await;
    platform_row_at(&e, "tenant.create", Some(a), "2026-10-01T10:00:00Z", "ok").await;
    let mut want = Vec::new();
    for h in 0..5 {
        want.push(
            platform_row_at(
                &e,
                "tenant.import",
                Some(a),
                &format!("2026-10-10T0{h}:00:00Z"),
                "ok",
            )
            .await,
        );
        platform_row_at(&e, "tenant.import", Some(b), "2026-10-10T05:00:00Z", "ok").await;
        platform_row_at(&e, "tenant.suspend", Some(a), "2026-10-10T05:00:00Z", "ok").await;
    }
    want.reverse();
    let base = format!("?tenant={a}&since=2026-10-10&family=creation&limit=2");
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let q = match &cursor {
            Some(c) => format!("{base}&before={c}"),
            None => base.clone(),
        };
        let (s, body) = send(
            &e.app,
            get(&format!("/api/platform/audit{q}"), Some(&super_admin())),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        for r in body["items"].as_array().unwrap() {
            assert_eq!(r["subject_tenant"], json!(a));
            assert_eq!(r["action"], "tenant.import");
            seen.push(r["id"].as_str().unwrap().parse::<i64>().unwrap());
        }
        match body["next"].as_str() {
            Some(n) => cursor = Some(n.to_string()),
            None => break,
        }
    }
    assert_eq!(
        seen, want,
        "every matching row once, newest first, across pages"
    );
}

#[tokio::test]
async fn platform_audit_bad_tenant_422() {
    let (e, _t) = audit_tenant().await;
    for bad in ["nope", "1234", "u-admin"] {
        let (s, b) = send(
            &e.app,
            get(
                &format!("/api/platform/audit?tenant={bad}"),
                Some(&super_admin()),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
        assert_eq!(b["error"]["code"], "invalid_tenant_id");
    }
}

#[tokio::test]
async fn platform_audit_bad_since_422() {
    let (e, _t) = audit_tenant().await;
    for bad in [
        "yesterday",
        "2026-13-01",
        "20261020",
        "2026-10-20T25:00:00Z",
    ] {
        let (s, b) = send(
            &e.app,
            get(
                &format!("/api/platform/audit?since={bad}"),
                Some(&super_admin()),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
        assert_eq!(b["error"]["code"], "invalid_since");
    }
}

#[tokio::test]
async fn archived_tenant_history_still_filterable() {
    let (e, a) = audit_tenant().await;
    let gone = platform_row_at(&e, "tenant.offboard", Some(a), "2026-10-05T10:00:00Z", "ok").await;
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(a)
        .execute(&e.owner)
        .await
        .unwrap();
    assert_eq!(platform_ids(&e, &format!("?tenant={a}")).await, [gone]);
    // the summary stays the whole log
    let (_, s) = send(
        &e.app,
        get(
            &format!("/api/platform/audit/summary?tenant={}", Uuid::new_v4()),
            Some(&super_admin()),
        ),
    )
    .await;
    assert_eq!(s["total"], 1);
}

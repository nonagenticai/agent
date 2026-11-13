//! B1 T9: the provisioning outbox (crate::provision): enqueue, supersede, the fenced drainer with scripted senders
//! (no network), the response classes, FIFO per (tenant, target), `require_provisioned` and the offboard hook. Real
//! Postgres 18 (one container for this binary).
use crate::common;

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use agent_api::audit::Actor;
use agent_api::provision::{
    NewRow, OutboxRow, SendResult, Sender, Senders, Target, drain_once, enqueue,
    on_tenant_offboarded, require_provisioned,
};
use agent_api::state::AppState;
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::{TestEnv, insert_tenant};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// Answers from a script, then `then` forever; records every row it was asked to send.
struct TestSender {
    op: &'static str,
    script: Mutex<VecDeque<SendResult>>,
    then: SendResult,
    calls: Mutex<Vec<Uuid>>,
    /// Re-lease the row while "sending" (another drainer took it over): the finish must be discarded.
    steal: Option<PgPool>,
}

impl TestSender {
    fn new(op: &'static str, script: Vec<SendResult>, then: SendResult) -> Arc<Self> {
        Arc::new(Self {
            op,
            script: Mutex::new(script.into()),
            then,
            calls: Mutex::new(Vec::new()),
            steal: None,
        })
    }

    fn calls(&self) -> Vec<Uuid> {
        self.calls.lock().unwrap().clone()
    }
}

impl Sender for TestSender {
    fn op(&self) -> &'static str {
        self.op
    }

    fn send<'a>(&'a self, _st: &'a AppState, row: &'a OutboxRow) -> BoxFuture<'a, SendResult> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(row.id);
            if let Some(db) = &self.steal {
                sqlx::query(
                    "UPDATE provision_outbox SET lease_token = gen_random_uuid() WHERE id = $1",
                )
                .bind(row.id)
                .execute(db)
                .await
                .unwrap();
            }
            let next = self.script.lock().unwrap().pop_front();
            next.unwrap_or_else(|| self.then.clone())
        })
    }
}

fn ok() -> SendResult {
    SendResult::Done(json!({"ok": true}))
}

fn http(status: u16, retry_after: Option<u64>) -> SendResult {
    SendResult::Http {
        status,
        code: None,
        retry_after,
        body: json!({"message": "upstream said no"}),
    }
}

fn with_senders(e: &TestEnv, senders: Vec<Arc<dyn Sender>>) -> AppState {
    let mut st = e.state.clone();
    let mut s = Senders::default();
    for x in senders {
        s.register(x);
    }
    st.senders = Arc::new(s);
    st
}

fn admin() -> Actor {
    Actor::member("u-admin", false)
}

async fn enqueue_row(
    st: &AppState,
    t: Uuid,
    op: &'static str,
    target: Target,
    key: &str,
    payload: Value,
) -> Uuid {
    let mut tx = begin_scoped(&st.db, t).await.unwrap();
    let id = enqueue(
        &mut tx,
        t,
        NewRow {
            target,
            op,
            dedupe_key: key.to_string(),
            payload,
        },
        &admin(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

#[derive(Debug)]
#[allow(dead_code)]
struct Row {
    status: String,
    attempts: i32,
    code: Option<String>,
    http: Option<i32>,
    next: DateTime<Utc>,
    token: Option<Uuid>,
    payload: Value,
}

async fn row(e: &TestEnv, id: Uuid) -> Row {
    let (status, attempts, code, http, next, token, payload) = sqlx::query_as(
        "SELECT status::text, attempts, last_error_code, last_http_status, next_attempt_at, lease_token, payload \
           FROM provision_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    Row {
        status,
        attempts,
        code,
        http,
        next,
        token,
        payload,
    }
}

async fn audits(e: &TestEnv, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = $1")
        .bind(action)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn tenant(e: &TestEnv, name: &str) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, name).await;
    t
}

/// Make every pending row due now.
async fn due_now(e: &TestEnv) {
    sqlx::query("UPDATE provision_outbox SET next_attempt_at = now() WHERE status = 'pending'")
        .execute(&e.owner)
        .await
        .unwrap();
}

#[tokio::test]
async fn enqueue_writes_one_pending_row_and_audits() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    let id = enqueue_row(
        &e.state,
        t,
        "licence_ensure",
        Target::Licences,
        "licences:licence_ensure",
        json!({"bundle": "agent"}),
    )
    .await;
    let r = row(&e, id).await;
    assert_eq!((r.status.as_str(), r.attempts), ("pending", 0));
    let (key, by): (String, String) =
        sqlx::query_as("SELECT dedupe_key, created_by FROM provision_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (key.as_str(), by.as_str()),
        ("licences:licence_ensure", "u-admin")
    );
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'provision.enqueued' AND resource_id = $1",
    )
    .bind(id.to_string())
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        details,
        json!({"op": "licence_ensure", "target": "licences", "dedupe_key": "licences:licence_ensure"})
    );
}

#[tokio::test]
async fn second_enqueue_replaces_pending_payload() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    let a = enqueue_row(
        &e.state,
        t,
        "connections_put",
        Target::StrataRag,
        "strata_rag:connections_put",
        json!({"v": 1}),
    )
    .await;
    let b = enqueue_row(
        &e.state,
        t,
        "connections_put",
        Target::StrataRag,
        "strata_rag:connections_put",
        json!({"v": 2}),
    )
    .await;
    assert_eq!(a, b, "one pending row per key");
    assert_eq!(row(&e, a).await.payload, json!({"v": 2}));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM provision_outbox")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn enqueue_supersedes_failed_and_dead() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    for status in ["failed", "dead"] {
        sqlx::query(
            "INSERT INTO provision_outbox (tenant_id, target, op, dedupe_key, status, created_by) \
             VALUES ($1, 'strata_rag', 'store_ensure', 'strata_rag:store_ensure:faq', $2::provision_status, 't')",
        )
        .bind(t)
        .bind(status)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let id = enqueue_row(
        &e.state,
        t,
        "store_ensure",
        Target::StrataRag,
        "strata_rag:store_ensure:faq",
        json!({"key": "faq"}),
    )
    .await;
    let statuses: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, status::text FROM provision_outbox ORDER BY created_at, id")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(statuses.len(), 3);
    for (rid, s) in &statuses {
        if *rid == id {
            assert_eq!(s, "pending");
        } else {
            assert_eq!(s, "superseded");
        }
    }
}

#[tokio::test]
async fn enqueue_rejects_secret_payload() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    for payload in [
        json!({"embed": {"kind": "openai", "api_key": "x"}}),
        json!({"note": "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4In0.c2ln"}),
        json!({"contact": "ann@globex.com"}),
    ] {
        let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
        let err = enqueue(
            &mut tx,
            t,
            NewRow {
                target: Target::StrataRag,
                op: "connections_put",
                dedupe_key: "strata_rag:connections_put".into(),
                payload: payload.clone(),
            },
            &admin(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.status_and_code(),
            (StatusCode::UNPROCESSABLE_ENTITY, "secret_rejected"),
            "{payload}"
        );
        assert!(
            !err.to_string().contains("ann@globex.com"),
            "the value is not echoed"
        );
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM provision_outbox")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(audits(&e, "provision.enqueued").await, 0);
}

#[tokio::test]
async fn unregistered_op_is_never_claimed() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    let sender = TestSender::new("licence_ensure", vec![], ok());
    let st = with_senders(&e, vec![sender.clone()]);
    let orphan = enqueue_row(
        &st,
        t,
        "connections_put",
        Target::StrataRag,
        "strata_rag:connections_put",
        json!({}),
    )
    .await;
    let sent = enqueue_row(
        &st,
        t,
        "licence_ensure",
        Target::Licences,
        "licences:licence_ensure",
        json!({}),
    )
    .await;
    let r = drain_once(&st).await;
    assert_eq!((r.claimed, r.succeeded), (1, 1), "{r:?}");
    assert_eq!(sender.calls(), [sent]);
    let o = row(&e, orphan).await;
    assert_eq!(
        (o.status.as_str(), o.attempts),
        ("pending", 0),
        "visible, never claimed and failed"
    );
    assert_eq!(row(&e, sent).await.status, "succeeded");
    assert_eq!(audits(&e, "provision.succeeded").await, 1);
    // and with no sender at all, nothing is claimed (production registers B3's senders: not this state)
    assert_eq!(drain_once(&with_senders(&e, vec![])).await.claimed, 0);
}

/// Drain until nothing is pending or in flight (bounded).
async fn drain_all(st: &AppState, owner: &PgPool) {
    for _ in 0..200 {
        let r = drain_once(st).await;
        if r.claimed == 0 {
            let left: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM provision_outbox WHERE status IN ('pending', 'in_flight')",
            )
            .fetch_one(owner)
            .await
            .unwrap();
            if left == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    panic!("the outbox did not drain");
}

#[tokio::test]
async fn two_drainers_send_each_row_once() {
    let e = common::env().await;
    let sender = TestSender::new("store_ensure", vec![], ok());
    let st = with_senders(&e, vec![sender.clone()]);
    let mut ids = HashSet::new();
    for n in 0..5 {
        let t = tenant(&e, &format!("Tenant {n}")).await;
        for k in 0..4 {
            ids.insert(
                enqueue_row(
                    &st,
                    t,
                    "store_ensure",
                    Target::StrataRag,
                    &format!("strata_rag:store_ensure:k{k}"),
                    json!({"key": format!("k{k}")}),
                )
                .await,
            );
        }
    }
    assert_eq!(ids.len(), 20);
    tokio::join!(drain_all(&st, &e.owner), drain_all(&st, &e.owner));
    let calls = sender.calls();
    assert_eq!(calls.len(), 20, "each row sent once: {calls:?}");
    assert_eq!(calls.iter().copied().collect::<HashSet<_>>(), ids);
    let done: i64 =
        sqlx::query_scalar("SELECT count(*) FROM provision_outbox WHERE status = 'succeeded'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(done, 20);
}

#[tokio::test]
async fn lost_lease_finish_is_discarded() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    let sender = Arc::new(TestSender {
        op: "licence_ensure",
        script: Mutex::new(VecDeque::new()),
        then: ok(),
        calls: Mutex::new(Vec::new()),
        steal: Some(e.owner.clone()),
    });
    let st = with_senders(&e, vec![sender.clone()]);
    let id = enqueue_row(
        &st,
        t,
        "licence_ensure",
        Target::Licences,
        "licences:licence_ensure",
        json!({}),
    )
    .await;
    let r = drain_once(&st).await;
    assert_eq!((r.claimed, r.discarded, r.succeeded), (1, 1, 0), "{r:?}");
    let row = row(&e, id).await;
    assert_eq!(row.status, "in_flight", "the new holder's lease stands");
    assert!(row.token.is_some());
    assert_eq!(audits(&e, "provision.succeeded").await, 0);
}

#[tokio::test]
async fn retry_after_429_not_counted() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    let sender = TestSender::new("licence_ensure", vec![http(429, Some(120))], ok());
    let st = with_senders(&e, vec![sender.clone()]);
    let id = enqueue_row(
        &st,
        t,
        "licence_ensure",
        Target::Licences,
        "licences:licence_ensure",
        json!({}),
    )
    .await;
    let r = drain_once(&st).await;
    assert_eq!(r.retried, 1, "{r:?}");
    let row = row(&e, id).await;
    assert_eq!(
        (row.status.as_str(), row.attempts),
        ("pending", 0),
        "not counted"
    );
    assert_eq!(row.code.as_deref(), Some("rate_limited"));
    assert_eq!(row.http, Some(429));
    assert!(
        row.next >= Utc::now() + chrono::Duration::seconds(110),
        "{:?}",
        row.next
    );
    assert_eq!(
        drain_once(&st).await.claimed,
        0,
        "not due before Retry-After"
    );
}

#[tokio::test]
async fn twelve_5xx_end_dead() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    let sender = TestSender::new("licence_ensure", vec![], http(503, None));
    let st = with_senders(&e, vec![sender.clone()]);
    let id = enqueue_row(
        &st,
        t,
        "licence_ensure",
        Target::Licences,
        "licences:licence_ensure",
        json!({}),
    )
    .await;
    for attempt in 1..=12 {
        due_now(&e).await;
        let r = drain_once(&st).await;
        assert_eq!(r.claimed, 1, "attempt {attempt}: {r:?}");
        if attempt < 12 {
            assert_eq!(r.retried, 1, "attempt {attempt}");
            let row = row(&e, id).await;
            assert_eq!((row.status.as_str(), row.attempts), ("pending", attempt));
        } else {
            assert_eq!(r.dead, 1);
        }
    }
    let row = row(&e, id).await;
    assert_eq!((row.status.as_str(), row.attempts), ("dead", 12));
    assert_eq!(row.code.as_deref(), Some("upstream_5xx"));
    assert_eq!(audits(&e, "provision.dead").await, 1);
    due_now(&e).await;
    assert_eq!(drain_once(&st).await.claimed, 0, "dead is terminal");
    let last_error: String =
        sqlx::query_scalar("SELECT last_error FROM provision_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(last_error.contains("503"), "{last_error}");
}

#[tokio::test]
async fn per_tenant_target_fifo() {
    let e = common::env().await;
    let a = tenant(&e, "Alpha").await;
    let b = tenant(&e, "Beta").await;
    let ensure = TestSender::new("licence_ensure", vec![http(503, None)], ok());
    let revoke = TestSender::new("licence_revoke", vec![], ok());
    let st = with_senders(&e, vec![ensure.clone(), revoke.clone()]);
    let first = enqueue_row(
        &st,
        a,
        "licence_ensure",
        Target::Licences,
        "licences:licence_ensure",
        json!({}),
    )
    .await;
    let second = enqueue_row(
        &st,
        a,
        "licence_revoke",
        Target::Licences,
        "licences:licence_revoke:agent",
        json!({}),
    )
    .await;
    let other = enqueue_row(
        &st,
        b,
        "licence_revoke",
        Target::Licences,
        "licences:licence_revoke:agent",
        json!({}),
    )
    .await;
    // pass 1: A's first row (fails, backs off) and B's row; A's second waits behind A's first
    let r = drain_once(&st).await;
    assert_eq!((r.claimed, r.retried, r.succeeded), (2, 1, 1), "{r:?}");
    assert_eq!(revoke.calls(), [other]);
    assert_eq!(row(&e, second).await.attempts, 0);
    // pass 2 (first row due again): it succeeds; the second still waits for this pass to finish
    due_now(&e).await;
    let r = drain_once(&st).await;
    assert_eq!((r.claimed, r.succeeded), (1, 1), "{r:?}");
    assert_eq!(ensure.calls(), [first, first]);
    // pass 3: now the second
    let r = drain_once(&st).await;
    assert_eq!((r.claimed, r.succeeded), (1, 1), "{r:?}");
    assert_eq!(revoke.calls(), [other, second]);
}

#[tokio::test]
async fn require_provisioned_pending_409_failed_503() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    let db = e.state.db.clone();
    let check = || {
        let db = db.clone();
        async move {
            let mut tx = begin_scoped(&db, t).await.unwrap();
            require_provisioned(&mut tx, Target::StrataRag).await
        }
    };
    check().await.expect("nothing queued: usable");
    let id = enqueue_row(
        &e.state,
        t,
        "connections_put",
        Target::StrataRag,
        "strata_rag:connections_put",
        json!({}),
    )
    .await;
    let err = check().await.unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::CONFLICT, "provision_pending")
    );
    sqlx::query("UPDATE provision_outbox SET status = 'failed' WHERE id = $1")
        .bind(id)
        .execute(&e.owner)
        .await
        .unwrap();
    let err = check().await.unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::SERVICE_UNAVAILABLE, "provision_failed")
    );
    let body = axum::response::IntoResponse::into_response(err);
    assert_eq!(body.status(), StatusCode::SERVICE_UNAVAILABLE);
    // another target is unaffected
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    require_provisioned(&mut tx, Target::StrataIngest)
        .await
        .expect("strata_ingest has nothing queued");
    drop(tx);
    // a recorded success makes it usable again
    sqlx::query("INSERT INTO provision_state (tenant_id, target, last_success_at) VALUES ($1, 'strata_rag', now())")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    check().await.expect("last success recorded");
}

#[tokio::test]
async fn offboard_hook_cancels_pending_data_plane_rows() {
    let e = common::env().await;
    let t = tenant(&e, "Alpha").await;
    let put = enqueue_row(
        &e.state,
        t,
        "connections_put",
        Target::StrataRag,
        "strata_rag:connections_put",
        json!({}),
    )
    .await;
    let store = enqueue_row(
        &e.state,
        t,
        "store_ensure",
        Target::StrataRag,
        "strata_rag:store_ensure:faq",
        json!({}),
    )
    .await;
    let revoke = enqueue_row(
        &e.state,
        t,
        "licence_revoke",
        Target::Licences,
        "licences:licence_revoke:agent",
        json!({}),
    )
    .await;
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let mut cancelled = on_tenant_offboarded(&mut tx, t, &Actor::member("u-super", true))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    cancelled.sort();
    let mut want = vec![put, store];
    want.sort();
    assert_eq!(cancelled, want);
    assert_eq!(row(&e, put).await.status, "cancelled");
    assert_eq!(row(&e, store).await.status, "cancelled");
    assert_eq!(
        row(&e, revoke).await.status,
        "pending",
        "licence revokes still run"
    );
    assert_eq!(audits(&e, "provision.cancelled").await, 2);
}

// ---- B1 T15: the overview's Integrations panel and blocks, Retry and Cancel over HTTP -----------------------------

use axum::body::Body;
use axum::http::Request;
use common::{get as get_req, insert_user, json as req_json, person, send, super_admin};

/// A tenant with a tenant_admin `u-admin` and a tenant_user `u-user`.
async fn http_tenant(e: &TestEnv) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    insert_user(&e.owner, "u-user", t, "tenant_user").await;
    t
}

/// An outbox row written as the owner, `age_min` minutes old.
async fn raw_row(
    e: &TestEnv,
    t: Uuid,
    target: &str,
    key: &str,
    status: &str,
    age_min: i32,
) -> Uuid {
    let op = if target == "licences" {
        "licence_ensure"
    } else {
        "store_ensure"
    };
    sqlx::query_scalar(
        "INSERT INTO provision_outbox (tenant_id, target, op, dedupe_key, status, attempts, last_error_code, \
                                       created_by, created_at, next_attempt_at) \
         VALUES ($1, $2, $3, $4, $5::provision_status, 5, \
                 CASE WHEN $5 IN ('failed', 'dead') THEN 'upstream_5xx' END, 'test', \
                 now() - make_interval(mins => $6), now() + interval '1 hour') RETURNING id",
    )
    .bind(t)
    .bind(target)
    .bind(op)
    .bind(key)
    .bind(status)
    .bind(age_min)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

fn as_sa(mut r: Request<Body>, t: Uuid) -> Request<Body> {
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    r
}

async fn overview(e: &TestEnv) -> Value {
    let (s, b) = send(
        &e.app,
        get_req("/api/tenant/overview", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn panel<'a>(b: &'a Value, target: &str) -> &'a Value {
    b["integrations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["target"] == target)
        .unwrap()
}

#[tokio::test]
async fn overview_current_is_oldest_open_row() {
    let e = common::env().await;
    let t = http_tenant(&e).await;
    let old = raw_row(&e, t, "strata_rag", "k1", "failed", 120).await;
    raw_row(&e, t, "strata_rag", "k2", "pending", 60).await;
    raw_row(&e, t, "strata_rag", "k3", "succeeded", 10).await;
    let b = overview(&e).await;
    let rag = panel(&b, "strata_rag");
    assert_eq!(rag["label"], "Knowledge");
    assert_eq!(
        rag["current"]["id"],
        old.to_string(),
        "FIFO: the oldest open row"
    );
    assert_eq!(rag["chip"], "failed");
    assert_eq!(rag["history"].as_array().unwrap().len(), 3);
    assert_eq!(
        rag["history"][0]["dedupe_key"], "k3",
        "history newest first"
    );
    assert!(rag["current"].get("payload").is_none());
    let lic = panel(&b, "licences");
    assert_eq!(lic["chip"], "ready");
    assert!(lic["current"].is_null());
    assert_eq!(b["members"]["total"], 2);
    assert_eq!(b["members"]["admins"], 1);
    assert_eq!(b["tenant"]["id"], t.to_string());
    assert_eq!(b["licence"]["app"], "agent");
    assert_eq!(b["licence"]["status"], "none");
}

#[tokio::test]
async fn blocks_follow_chips() {
    let e = common::env().await;
    let t = http_tenant(&e).await;
    raw_row(&e, t, "strata_rag", "k1", "pending", 5).await;
    raw_row(&e, t, "strata_ingest", "k2", "dead", 5).await;
    raw_row(&e, t, "licences", "k3", "failed", 5).await;
    // licences succeeded before: its failed row does not block data-plane calls
    sqlx::query(
        "INSERT INTO provision_state (tenant_id, target, last_success_at) VALUES ($1, 'licences', now())",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let b = overview(&e).await;
    assert_eq!(
        b["blocks"],
        json!([
            {"code": "provision_pending", "status": 409, "targets": ["strata_rag"]},
            {"code": "provision_failed", "status": 503, "targets": ["strata_ingest"]},
        ])
    );
    assert_eq!(panel(&b, "strata_ingest")["chip"], "dead");
    assert_eq!(panel(&b, "licences")["chip"], "failed");
    assert!(panel(&b, "licences")["last_success_at"].is_string());
}

#[tokio::test]
async fn retry_resets_attempts_and_due_now() {
    let e = common::env().await;
    let t = http_tenant(&e).await;
    let id = raw_row(&e, t, "strata_rag", "k1", "failed", 5).await;
    let (s, b) = send(
        &e.app,
        req_json(
            "POST",
            &format!("/api/tenant/provisioning/{id}/retry"),
            Some(&person("u-admin", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "pending");
    assert_eq!(b["attempts"], 0);
    let due: bool = sqlx::query_scalar(
        "SELECT next_attempt_at <= now() AND completed_at IS NULL FROM provision_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(due, "due now");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'provision.retried' AND resource_id = $1",
    )
    .bind(id.to_string())
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn retry_in_flight_409() {
    let e = common::env().await;
    let t = http_tenant(&e).await;
    let id = raw_row(&e, t, "strata_rag", "k1", "in_flight", 5).await;
    let admin = person("u-admin", &[]);
    let (s, b) = send(
        &e.app,
        req_json(
            "POST",
            &format!("/api/tenant/provisioning/{id}/retry"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "not_retryable");
    let (s, b) = send(
        &e.app,
        req_json(
            "POST",
            &format!("/api/tenant/provisioning/{}/retry", Uuid::new_v4()),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "provision_not_found");
    let (s, _) = send(
        &e.app,
        req_json(
            "POST",
            &format!("/api/tenant/provisioning/{id}/retry"),
            Some(&person("u-user", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cancel_needs_tenant_admin() {
    // D88 §R.1: cancelProvision is a tenant_admin write (it was super_admin-only before T21).
    let e = common::env().await;
    let t = http_tenant(&e).await;
    let id = raw_row(&e, t, "strata_rag", "k1", "failed", 5).await;
    let uri = format!("/api/tenant/provisioning/{id}/cancel");
    let (s, b) = send(
        &e.app,
        req_json("POST", &uri, Some(&person("u-user", &[])), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "forbidden");
    assert_eq!(b["error"]["missing_role"], "tenant_admin");
    let (s, b) = send(
        &e.app,
        as_sa(req_json("POST", &uri, Some(&super_admin()), json!({})), t),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "cancelled");
    let (s, b) = send(
        &e.app,
        req_json("POST", &uri, Some(&person("u-admin", &[])), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"]["code"], "not_cancellable");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'provision.cancelled' AND platform_actor",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn platform_retry_finds_the_rows_tenant() {
    let e = common::env().await;
    let t = http_tenant(&e).await;
    let id = raw_row(&e, t, "strata_ingest", "k1", "dead", 5).await;
    let (s, b) = send(
        &e.app,
        req_json(
            "POST",
            &format!("/api/tenants/{t}/provisioning/{id}/retry"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["id"], id.to_string());
    assert_eq!(b["status"], "pending");
    assert_eq!(b["tenant_id"], t.to_string());
    assert!(b.get("last_success_at").is_some(), "the platform shape");
    let (s, b) = send(
        &e.app,
        req_json(
            "POST",
            &format!("/api/tenants/{t}/provisioning/{id}/retry"),
            Some(&person("u-admin", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
}

#[tokio::test]
async fn platform_retry_other_tenants_row_with_wrong_path_404() {
    let e = common::env().await;
    let a = http_tenant(&e).await;
    let b_t = Uuid::new_v4();
    insert_tenant(&e.owner, b_t, "Beta").await;
    let row_b = raw_row(&e, b_t, "strata_rag", "k1", "failed", 5).await;
    let (s, b) = send(
        &e.app,
        req_json(
            "POST",
            &format!("/api/tenants/{a}/provisioning/{row_b}/retry"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "not_found");
    let status: String =
        sqlx::query_scalar("SELECT status::text FROM provision_outbox WHERE id = $1")
            .bind(row_b)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(status, "failed", "nothing changed");
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(b_t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        req_json(
            "POST",
            &format!("/api/tenants/{b_t}/provisioning/{row_b}/cancel"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "tenant_archived");
}

//! Durable MCP tasks (migration 0240, `mcp_client::store`) on a real Postgres, against a stub MCP server (the external
//! boundary) that offers the tasks extension. The poller runs as `agent_runtime` (the only role that may EXECUTE
//! `agent_mcp_task_claim`); the store calls the API makes run as `agent_app` (`e.state.db`, RLS applies).
//! Ported from care-bus `mcp_tasks.rs`; the research-run parts are not ported.
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_api::connectors::tasks::TaskClients;
use agent_api::error::ApiError;
use agent_api::mcp_client::client::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
use agent_api::mcp_client::store::{
    self, AnswerInput, NewTask, TaskPoller, mark_delivered, record_started, undelivered_outcomes,
};
use agent_api::mcp_client::tasks::{StaffActor, TaskHandle};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common;

const TASKS: &str = "io.modelcontextprotocol/tasks";
const KEY: &str = "t-ask:00000000000000aa";

type Seen = Arc<Mutex<Vec<Value>>>;

/// The stub server's knobs and log.
#[derive(Clone, Default)]
struct Stub {
    seen: Seen,
    /// `tasks/update` and `tasks/cancel` answer HTTP 500 while set.
    fail_sends: Arc<AtomicBool>,
    cancelled: Arc<Mutex<HashSet<String>>>,
    /// While set, t-ask's input is re-opened as fr refusing the answer with this code.
    refuse: Arc<Mutex<Option<String>>>,
}

impl Stub {
    fn of(&self, method: &str) -> Vec<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b["method"] == method)
            .cloned()
            .collect()
    }
}

fn task(id: &str, status: &str) -> Value {
    json!({"resultType": "complete", "taskId": id, "status": status, "createdAt": "2026-10-30T10:00:00Z",
           "lastUpdatedAt": "2026-10-30T10:00:01Z", "ttlMs": 86_400_000, "pollIntervalMs": 2000})
}

/// `tasks/get`: t-work working, t-ask input_required (one approval), t-done completed, t-cancel working until
/// `tasks/cancel` reached the server and then cancelled, anything else -32602.
async fn stub_mcp() -> (Arc<HttpMcp>, Stub) {
    let stub = Stub::default();
    let s = stub.clone();
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::any(move |method: Method, body: String| {
            let s = s.clone();
            async move {
                if method != Method::POST {
                    return StatusCode::METHOD_NOT_ALLOWED.into_response();
                }
                let req: Value = serde_json::from_str(&body).unwrap();
                s.seen.lock().unwrap().push(req.clone());
                let id = req["id"].clone();
                let ok = |r: Value| json!({"jsonrpc": "2.0", "id": id, "result": r});
                let m = req["method"].as_str().unwrap_or("");
                let tid = req["params"]["taskId"].as_str().unwrap_or("").to_string();
                let known = ["t-work", "t-ask", "t-done", "t-cancel", "t-abandoned"]
                    .contains(&tid.as_str());
                if matches!(m, "tasks/update" | "tasks/cancel")
                    && known
                    && s.fail_sends.load(Ordering::SeqCst)
                {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                if m == "tasks/cancel" && known {
                    s.cancelled.lock().unwrap().insert(tid.clone());
                }
                let reply = match m {
                    "server/discover" => ok(json!({"resultType": "complete", "supportedVersions": ["2026-07-28"], "ttlMs": 0, "cacheScope": "private",
                        "capabilities": {"tools": {}, "extensions": {TASKS: {}}},
                        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "stub", "version": "0"}}})),
                    "tasks/get" | "tasks/update" | "tasks/cancel" if !known => json!({
                        "jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "unknown task"}}),
                    "tasks/update" | "tasks/cancel" => ok(json!({"resultType": "complete"})),
                    "tasks/get" => ok(match tid.as_str() {
                        "t-ask" => {
                            let mut t = task(&tid, "input_required");
                            t["statusMessage"] = match s.refuse.lock().unwrap().as_deref() {
                                Some(code) => json!(format!("fr refused the answer: {code}")),
                                None => json!("waiting for approval"),
                            };
                            t["inputRequests"] = json!({KEY: {"method": "elicitation/create", "params": {
                                "message": "Merge the release branch?",
                                "requestedSchema": {"type": "object", "required": ["decision"], "properties": {
                                    "decision": {"type": "string", "enum": ["approved", "rejected"]},
                                    "reason": {"type": "string"}, "comment": {"type": "string"}}}}}});
                            t
                        }
                        "t-done" => {
                            let mut t = task(&tid, "completed");
                            t["result"] = json!({"content": [], "isError": false, "structuredContent":
                                {"run_id": "r-9", "status": "done", "result": {"pr": 12}, "error": null}});
                            t
                        }
                        "t-cancel" if s.cancelled.lock().unwrap().contains("t-cancel") => {
                            task(&tid, "cancelled")
                        }
                        _ => task(&tid, "working"),
                    }),
                    _ => return StatusCode::ACCEPTED.into_response(),
                };
                axum::Json(reply).into_response()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let creds = TenantCredentials::new(FnSource(|_tenant: Uuid, _fresh: bool| async move {
        McpCredentials::new("lic", "bind", Instant::now() + Duration::from_secs(3600))
    }));
    (
        Arc::new(HttpMcp::new(format!("http://{addr}/mcp"), creds)),
        stub,
    )
}

/// A tenant and one job (owner pool: RLS does not apply).
async fn world(e: &common::TestEnv, name: &str) -> (Uuid, Uuid) {
    let tenant = Uuid::new_v4();
    common::insert_tenant(&e.owner, tenant, name).await;
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'mcp_job', 'MCP', '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    (tenant, job)
}

fn handle(id: &str) -> TaskHandle {
    TaskHandle {
        task_id: id.into(),
        status: "working".into(),
        status_message: None,
        poll_interval_ms: Some(2000),
        ttl_ms: Some(86_400_000),
    }
}

fn origin<'a>(tenant: Uuid, job: Uuid, case_id: Option<&'a str>) -> NewTask<'a> {
    NewTask {
        tenant,
        job,
        operation: "fr.project.start",
        idempotency_key: "idem-1",
        case_id,
        created_by: "agent",
        origin: "platform",
        instance: None,
    }
}

/// Records `id` through the store as the API role and makes it due now.
async fn start(e: &common::TestEnv, tenant: Uuid, job: Uuid, id: &str, case: Option<&str>) {
    record_started(
        &e.state.db,
        &origin(tenant, job, case),
        "fr_project_start",
        &handle(id),
    )
    .await
    .unwrap();
    make_due(&e.owner, id).await;
}

async fn make_due(owner: &PgPool, id: &str) {
    sqlx::query("UPDATE mcp_tasks SET poll_after = now() - interval '1 second' WHERE task_id = $1")
        .bind(id)
        .execute(owner)
        .await
        .unwrap();
}

#[derive(sqlx::FromRow, Debug)]
struct Row {
    status: String,
    status_message: Option<String>,
    result: Option<Value>,
    last_detail: Option<Value>,
    terminal: bool,
    cancel_requested: bool,
    cancel_sent: bool,
    wait_s: f64,
}

async fn row(e: &common::TestEnv, id: &str) -> Row {
    sqlx::query_as(
        "SELECT status, status_message, result, last_detail, terminal_at IS NOT NULL AS terminal,
                cancel_requested, cancel_sent, extract(epoch FROM poll_after - now())::float8 AS wait_s
           FROM mcp_tasks WHERE task_id = $1",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// The task backends as the platform builds them, with this stub as the platform MCP server.
fn clients(e: &common::TestEnv, mcp: &Arc<HttpMcp>) -> TaskClients {
    TaskClients {
        platform: Some(mcp.clone()),
        connector: e.state.task_clients().connector,
    }
}

async fn poller(e: &common::TestEnv, mcp: Arc<HttpMcp>) -> TaskPoller {
    TaskPoller {
        db: common::runtime_pool(&e.owner).await,
        clients: clients(e, &mcp),
        lease_s: 30,
        batch: 20,
        answer_timeout: Duration::from_secs(3600),
    }
}

fn input(key: &str, action: &str, content: Option<Value>) -> AnswerInput {
    AnswerInput {
        key: key.into(),
        action: action.into(),
        content,
    }
}

#[tokio::test]
async fn the_poller_stores_each_status_and_honours_the_poll_interval() {
    let e = common::env().await;
    let (t, j) = world(&e, "mcp-poll").await;
    let (mcp, stub) = stub_mcp().await;
    for id in ["t-work", "t-ask", "t-done", "t-gone"] {
        start(&e, t, j, id, None).await;
    }
    let p = poller(&e, mcp).await;
    assert_eq!(p.tick().await.unwrap(), 4);

    let w = row(&e, "t-work").await;
    assert_eq!(w.status, "working");
    assert!(!w.terminal);
    assert!(
        (1.0..=3.0).contains(&w.wait_s),
        "pollIntervalMs 2000 -> next poll in ~2 s: {w:?}"
    );

    let a = row(&e, "t-ask").await;
    assert_eq!(a.status, "input_required");
    assert_eq!(a.status_message.as_deref(), Some("waiting for approval"));
    assert!(a.last_detail.unwrap()["inputRequests"][KEY].is_object());

    // completed: the result is stored in the statement that made the row terminal (the table CHECK ties them).
    let d = row(&e, "t-done").await;
    assert_eq!(d.status, "completed");
    assert!(d.terminal);
    let r = d.result.expect("result stored with the terminal state");
    assert_eq!(r["outcome"], "completed");
    assert_eq!(r["result"]["result"], json!({"pr": 12}));

    // -32602: lost, terminal
    let g = row(&e, "t-gone").await;
    assert_eq!(g.status, "lost");
    assert!(g.terminal);
    assert_eq!(g.result.unwrap()["outcome"], "lost");

    // Nothing is due now; a terminal row is never claimed or polled again, even when forced due.
    assert_eq!(p.tick().await.unwrap(), 0);
    make_due(&e.owner, "t-done").await;
    make_due(&e.owner, "t-gone").await;
    assert_eq!(p.tick().await.unwrap(), 0);
    assert_eq!(stub.of("tasks/get").len(), 4);
}

/// A poller killed after claiming leaves the row leased; a second claim inside the lease gets nothing, once the lease
/// passes another poller claims it again.
#[tokio::test]
async fn a_row_claimed_by_a_killed_poller_is_claimed_again_after_its_lease() {
    let e = common::env().await;
    let (t, j) = world(&e, "mcp-restart").await;
    let (mcp, _) = stub_mcp().await;
    start(&e, t, j, "t-abandoned", None).await;
    let rt = common::runtime_pool(&e.owner).await;
    let claimed: Vec<(String,)> = sqlx::query_as("SELECT task_id FROM agent_mcp_task_claim(5, 10)")
        .fetch_all(&rt)
        .await
        .unwrap();
    assert_eq!(claimed, [("t-abandoned".to_string(),)]);
    let again: Vec<(String,)> = sqlx::query_as("SELECT task_id FROM agent_mcp_task_claim(5, 10)")
        .fetch_all(&rt)
        .await
        .unwrap();
    assert!(again.is_empty(), "inside the lease nobody else takes it");
    // ...and the process dies here.
    let p = poller(&e, mcp).await;
    assert_eq!(p.tick().await.unwrap(), 0);
    make_due(&e.owner, "t-abandoned").await; // the lease passes
    assert_eq!(p.tick().await.unwrap(), 1);
    assert_eq!(row(&e, "t-abandoned").await.status, "working");
}

#[tokio::test]
async fn only_the_runtime_role_may_claim() {
    let e = common::env().await;
    let err = sqlx::query("SELECT * FROM agent_mcp_task_claim(30, 10)")
        .execute(&e.state.db)
        .await
        .unwrap_err();
    assert_eq!(common::sqlstate(&err).as_deref(), Some("42501"), "{err}");
}

#[tokio::test]
async fn staff_answer_as_themselves_once_and_a_rejection_needs_a_reason() {
    let e = common::env().await;
    let (t, j) = world(&e, "mcp-answer").await;
    let (mcp, stub) = stub_mcp().await;
    start(&e, t, j, "t-ask", None).await;
    poller(&e, mcp.clone()).await.tick().await.unwrap();
    let actor = StaffActor {
        tenant: t,
        staff_user: "u-staff".into(),
    };

    // The open question is listed with its schema.
    let v = store::one(&e.state.db, t, j, "t-ask").await.unwrap();
    assert_eq!(v.status, "input_required");
    assert_eq!(v.questions.len(), 1);
    assert_eq!(v.questions[0]["key"], KEY);
    assert_eq!(
        v.questions[0]["requested_schema"]["required"],
        json!(["decision"])
    );

    // A rejection without a reason never reaches the server.
    let err = store::answer(
        &e.state.db,
        &clients(&e, &mcp),
        &actor,
        j,
        "t-ask",
        &input(KEY, "accept", Some(json!({"decision": "rejected"}))),
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, ApiError::Unprocessable("invalid_answer", _)),
        "{err:?}"
    );
    assert!(stub.of("tasks/update").is_empty());

    let content =
        json!({"decision": "rejected", "reason": "touches billing", "comment": "ask finance"});
    let r = store::answer(
        &e.state.db,
        &clients(&e, &mcp),
        &actor,
        j,
        "t-ask",
        &input(KEY, "accept", Some(content.clone())),
        &[],
    )
    .await
    .unwrap();
    assert!(r.sent && !r.already_answered);
    let upd = stub.of("tasks/update");
    assert_eq!(upd.len(), 1);
    assert_eq!(
        upd[0]["params"]["inputResponses"][KEY],
        json!({"action": "accept", "content": content})
    );
    assert_eq!(
        upd[0]["params"]["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("agent:{t}"), "on_behalf_of": "u-staff"})
    );
    let (sent,): (bool,) = sqlx::query_as(
        "SELECT sent_at IS NOT NULL FROM mcp_task_answers WHERE task_id = 't-ask' AND input_key = $1",
    )
    .bind(KEY)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(sent);
    // an answered question is no longer open
    let v = store::one(&e.state.db, t, j, "t-ask").await.unwrap();
    assert!(v.questions.is_empty());

    // A double submit is a no-op.
    let r = store::answer(
        &e.state.db,
        &clients(&e, &mcp),
        &actor,
        j,
        "t-ask",
        &input(KEY, "accept", Some(json!({"decision": "approved"}))),
        &[],
    )
    .await
    .unwrap();
    assert!(r.already_answered && !r.sent);
    assert_eq!(stub.of("tasks/update").len(), 1);

    // A question the task is not waiting on is a conflict.
    let err = store::answer(
        &e.state.db,
        &clients(&e, &mcp),
        &actor,
        j,
        "t-ask",
        &input("t-ask:ffff", "decline", None),
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, ApiError::Conflict("answered_elsewhere", _)),
        "{err:?}"
    );

    // A task that is not waiting (working) refuses an answer.
    start(&e, t, j, "t-work", None).await;
    let err = store::answer(
        &e.state.db,
        &clients(&e, &mcp),
        &actor,
        j,
        "t-work",
        &input(KEY, "decline", None),
        &[],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, ApiError::Conflict("not_waiting", _)),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_unsent_answer_is_retried_by_the_poller() {
    let e = common::env().await;
    let (t, j) = world(&e, "mcp-answer-retry").await;
    let (mcp, stub) = stub_mcp().await;
    start(&e, t, j, "t-ask", None).await;
    let p = poller(&e, mcp.clone()).await;
    p.tick().await.unwrap();
    let actor = StaffActor {
        tenant: t,
        staff_user: "u-staff".into(),
    };

    stub.fail_sends.store(true, Ordering::SeqCst);
    let r = store::answer(
        &e.state.db,
        &clients(&e, &mcp),
        &actor,
        j,
        "t-ask",
        &input(KEY, "accept", Some(json!({"decision": "approved"}))),
        &[],
    )
    .await
    .unwrap();
    assert!(!r.sent && !r.already_answered, "kept, not yet acknowledged");
    let unsent = |owner: PgPool| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT sent_at IS NULL FROM mcp_task_answers WHERE task_id = 't-ask'",
        )
        .fetch_one(&owner)
        .await
        .unwrap()
    };
    assert!(unsent(e.owner.clone()).await);

    // The server recovers; the poller sends the stored answer as the same human.
    stub.fail_sends.store(false, Ordering::SeqCst);
    make_due(&e.owner, "t-ask").await;
    assert_eq!(p.tick().await.unwrap(), 1);
    assert!(!unsent(e.owner.clone()).await);
    let upd = stub.of("tasks/update");
    let last = upd.last().unwrap();
    assert_eq!(
        last["params"]["inputResponses"][KEY],
        json!({"action": "accept", "content": {"decision": "approved"}})
    );
    assert_eq!(
        last["params"]["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("agent:{t}"), "on_behalf_of": "u-staff"})
    );
    // sent once acknowledged: not sent a third time
    let n = stub.of("tasks/update").len();
    make_due(&e.owner, "t-ask").await;
    p.tick().await.unwrap();
    assert_eq!(stub.of("tasks/update").len(), n);
}

#[tokio::test]
async fn cancel_flags_the_row_and_it_ends_only_when_the_server_says_cancelled() {
    let e = common::env().await;
    let (t, j) = world(&e, "mcp-cancel").await;
    let (mcp, stub) = stub_mcp().await;
    start(&e, t, j, "t-cancel", None).await;

    assert!(
        store::cancel(&e.state.db, &clients(&e, &mcp), t, j, "t-cancel")
            .await
            .unwrap()
    );
    assert_eq!(stub.of("tasks/cancel")[0]["params"]["taskId"], "t-cancel");
    let r = row(&e, "t-cancel").await;
    assert!(r.cancel_requested && r.cancel_sent);
    assert_eq!(
        r.status, "working",
        "stays open until tasks/get says cancelled"
    );
    assert!(!r.terminal);

    let p = poller(&e, mcp.clone()).await;
    assert_eq!(p.tick().await.unwrap(), 1);
    let r = row(&e, "t-cancel").await;
    assert_eq!(r.status, "cancelled");
    assert!(r.terminal);

    // a finished task cannot be cancelled again
    let err = store::cancel(&e.state.db, &clients(&e, &mcp), t, j, "t-cancel")
        .await
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Conflict("task_finished", _)),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_unsent_cancel_is_retried_by_the_poller() {
    let e = common::env().await;
    let (t, j) = world(&e, "mcp-cancel-retry").await;
    let (mcp, stub) = stub_mcp().await;
    start(&e, t, j, "t-cancel", None).await;
    stub.fail_sends.store(true, Ordering::SeqCst);
    assert!(
        !store::cancel(&e.state.db, &clients(&e, &mcp), t, j, "t-cancel")
            .await
            .unwrap()
    );
    let r = row(&e, "t-cancel").await;
    assert!(r.cancel_requested && !r.cancel_sent);

    stub.fail_sends.store(false, Ordering::SeqCst);
    make_due(&e.owner, "t-cancel").await;
    let p = poller(&e, mcp).await;
    p.tick().await.unwrap();
    assert!(row(&e, "t-cancel").await.cancel_sent);
    // the server now reports it cancelled on the next poll
    make_due(&e.owner, "t-cancel").await;
    p.tick().await.unwrap();
    assert_eq!(row(&e, "t-cancel").await.status, "cancelled");
}

#[tokio::test]
async fn an_unknown_task_is_lost_on_answer_and_cancel_paths() {
    let e = common::env().await;
    let (t, j) = world(&e, "mcp-lost").await;
    let (mcp, _) = stub_mcp().await;
    start(&e, t, j, "t-gone", None).await;
    assert!(
        !store::cancel(&e.state.db, &clients(&e, &mcp), t, j, "t-gone")
            .await
            .unwrap()
    );
    let r = row(&e, "t-gone").await;
    assert_eq!(r.status, "lost");
    assert!(r.terminal);
    assert_eq!(r.result.unwrap()["outcome"], "lost");
}

#[tokio::test]
async fn tenants_cannot_see_or_touch_each_others_tasks() {
    let e = common::env().await;
    let (a, ja) = world(&e, "mcp-iso-a").await;
    let (b, jb) = world(&e, "mcp-iso-b").await;
    let (mcp, stub) = stub_mcp().await;
    start(&e, a, ja, "t-ask", None).await;
    poller(&e, mcp.clone()).await.tick().await.unwrap();

    assert_eq!(
        store::list(&e.state.db, a, ja, false, 50)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store::list(&e.state.db, b, jb, false, 50)
            .await
            .unwrap()
            .is_empty()
    );
    // B naming A's job is still B's scope: nothing
    assert!(
        store::list(&e.state.db, b, ja, false, 50)
            .await
            .unwrap()
            .is_empty()
    );
    for (tenant, job) in [(b, jb), (b, ja)] {
        let err = store::one(&e.state.db, tenant, job, "t-ask")
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApiError::Missing("task_not_found", _)),
            "{err:?}"
        );
        let actor = StaffActor {
            tenant,
            staff_user: "u-b".into(),
        };
        let err = store::answer(
            &e.state.db,
            &clients(&e, &mcp),
            &actor,
            job,
            "t-ask",
            &input(KEY, "decline", None),
            &[],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ApiError::Missing("task_not_found", _)),
            "{err:?}"
        );
        let err = store::cancel(&e.state.db, &clients(&e, &mcp), tenant, job, "t-ask")
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApiError::Missing("task_not_found", _)),
            "{err:?}"
        );
        assert!(
            !mark_delivered(&e.state.db, tenant, job, "t-ask")
                .await
                .unwrap()
        );
    }
    assert!(stub.of("tasks/update").is_empty());
    assert!(stub.of("tasks/cancel").is_empty());
    let r = row(&e, "t-ask").await;
    assert!(!r.cancel_requested);
}

#[tokio::test]
async fn a_finished_tasks_outcome_is_handed_to_its_case_exactly_once() {
    let e = common::env().await;
    let (t, j) = world(&e, "mcp-deliver").await;
    let (mcp, _) = stub_mcp().await;
    start(&e, t, j, "t-done", Some("case-1")).await;
    start(&e, t, j, "t-work", Some("case-1")).await;
    start(&e, t, j, "t-gone", Some("case-2")).await;
    poller(&e, mcp).await.tick().await.unwrap();

    let u = undelivered_outcomes(&e.state.db, t, j, "case-1")
        .await
        .unwrap();
    assert_eq!(u.len(), 1, "the still-working task is not an outcome yet");
    assert_eq!(u[0].task_id, "t-done");
    assert_eq!(u[0].status, "completed");
    assert_eq!(u[0].idempotency_key, "idem-1");
    assert_eq!(u[0].result.as_ref().unwrap()["outcome"], "completed");
    let other = undelivered_outcomes(&e.state.db, t, j, "case-2")
        .await
        .unwrap();
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].status, "lost");

    assert!(mark_delivered(&e.state.db, t, j, "t-done").await.unwrap());
    assert!(
        !mark_delivered(&e.state.db, t, j, "t-done").await.unwrap(),
        "idempotent"
    );
    assert!(
        undelivered_outcomes(&e.state.db, t, j, "case-1")
            .await
            .unwrap()
            .is_empty()
    );
    // a non-terminal task cannot be marked delivered
    assert!(!mark_delivered(&e.state.db, t, j, "t-work").await.unwrap());
    assert_eq!(
        undelivered_outcomes(&e.state.db, t, j, "case-2")
            .await
            .unwrap()
            .len(),
        1
    );
}

// ---- escalation (owner 2026-10-31) ----------------------------------------------------------------------------------

#[derive(sqlx::FromRow, Debug)]
struct Esc {
    escalated: bool,
    reason: Option<String>,
    notify: bool,
}

async fn esc(e: &common::TestEnv, id: &str) -> Esc {
    sqlx::query_as(
        "SELECT escalated_at IS NOT NULL AS escalated, escalation_reason AS reason,
                escalated_at IS NOT NULL AND escalation_notified_at IS NULL AS notify
           FROM mcp_tasks WHERE task_id = $1",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// `(attempt, answered_by, outcome, refusal_code, sent)` of every answer to `key`, oldest first.
async fn answers(
    e: &common::TestEnv,
    id: &str,
) -> Vec<(i16, String, Option<String>, Option<String>, bool)> {
    sqlx::query_as(
        "SELECT attempt, answered_by, outcome, refusal_code, sent_at IS NOT NULL FROM mcp_task_answers
          WHERE task_id = $1 AND input_key = $2 ORDER BY attempt",
    )
    .bind(id)
    .bind(KEY)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

/// t-ask waiting, answered by the job for the customer (sent), then fr refuses it with `code` on the next poll.
async fn refused(code: &str) -> (common::TestEnv, Uuid, Uuid, Arc<HttpMcp>, Stub) {
    let e = common::env().await;
    let (t, j) = world(&e, &format!("mcp-refuse-{code}")).await;
    let (mcp, stub) = stub_mcp().await;
    start(&e, t, j, "t-ask", None).await;
    let p = poller(&e, mcp.clone()).await;
    p.tick().await.unwrap();
    let job = StaffActor::for_job(t, j);
    let r = store::answer(
        &e.state.db,
        &clients(&e, &mcp),
        &job,
        j,
        "t-ask",
        &input(KEY, "accept", Some(json!({"decision": "approved"}))),
        &["m-1".to_owned()],
    )
    .await
    .unwrap();
    assert!(r.sent);
    *stub.refuse.lock().unwrap() = Some(code.to_owned());
    make_due(&e.owner, "t-ask").await;
    p.tick().await.unwrap();
    (e, t, j, mcp, stub)
}

#[tokio::test]
async fn an_answer_fr_says_a_person_must_give_goes_to_staff_who_answer_it_again() {
    let (e, t, j, mcp, stub) = refused("agent_decision_not_allowed").await;
    let job = format!("job:{j}");
    assert_eq!(
        answers(&e, "t-ask").await,
        [(
            1,
            job.clone(),
            Some("refused".into()),
            Some("agent_decision_not_allowed".into()),
            true
        )]
    );
    let x = esc(&e, "t-ask").await;
    assert!(x.escalated && x.notify, "{x:?}");
    assert_eq!(
        x.reason.as_deref(),
        Some("refused:agent_decision_not_allowed")
    );
    // the customer's message stays linked to the job's (refused) answer
    let (src,): (Vec<String>,) = sqlx::query_as(
        "SELECT source_messages FROM mcp_task_answers WHERE task_id = 't-ask' AND attempt = 1",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(src, ["m-1"]);

    // the question is open again in the Tasks inbox; staff answer as themselves (attempt 2)
    let v = store::one(&e.state.db, t, j, "t-ask").await.unwrap();
    assert_eq!(v.questions.len(), 1, "{v:?}");
    assert_eq!(
        v.escalation_reason.as_deref(),
        Some("refused:agent_decision_not_allowed")
    );
    *stub.refuse.lock().unwrap() = None;
    let staff = StaffActor {
        tenant: t,
        staff_user: "u-staff".into(),
    };
    let r = store::answer(
        &e.state.db,
        &clients(&e, &mcp),
        &staff,
        j,
        "t-ask",
        &input(KEY, "accept", Some(json!({"decision": "approved"}))),
        &[],
    )
    .await
    .unwrap();
    assert!(r.sent && !r.already_answered);
    let a = answers(&e, "t-ask").await;
    assert_eq!(a[1], (2, "u-staff".into(), None, None, true));
    let upd = stub.of("tasks/update");
    assert_eq!(
        upd.last().unwrap()["params"]["_meta"]["ai.nonagentic/actor"]["on_behalf_of"],
        "u-staff"
    );
    // a standing answer is not answered twice
    let again = store::answer(
        &e.state.db,
        &clients(&e, &mcp),
        &staff,
        j,
        "t-ask",
        &input(KEY, "accept", Some(json!({"decision": "approved"}))),
        &[],
    )
    .await
    .unwrap();
    assert!(again.already_answered);
}

#[tokio::test]
async fn a_question_closed_elsewhere_in_fr_is_dropped_without_escalation() {
    let (e, t, j, _mcp, _stub) = refused("already_decided").await;
    assert_eq!(answers(&e, "t-ask").await[0].2.as_deref(), Some("dropped"));
    assert!(!esc(&e, "t-ask").await.escalated);
    // nothing left to answer
    assert!(
        store::one(&e.state.db, t, j, "t-ask")
            .await
            .unwrap()
            .questions
            .is_empty()
    );
}

#[tokio::test]
async fn a_misconfigured_refusal_is_not_escalated_but_staff_may_answer() {
    let (e, t, j, _mcp, _stub) = refused("actor_unresolved").await;
    assert_eq!(answers(&e, "t-ask").await[0].2.as_deref(), Some("refused"));
    assert!(!esc(&e, "t-ask").await.escalated);
    assert_eq!(
        store::one(&e.state.db, t, j, "t-ask")
            .await
            .unwrap()
            .questions
            .len(),
        1
    );
}

#[tokio::test]
async fn a_task_nobody_answers_in_time_goes_to_staff() {
    let e = common::env().await;
    let (t, j) = world(&e, "mcp-timeout").await;
    let (mcp, _stub) = stub_mcp().await;
    start(&e, t, j, "t-ask", None).await;
    let mut p = poller(&e, mcp).await;
    p.answer_timeout = Duration::from_secs(3600);
    p.tick().await.unwrap();
    assert!(!esc(&e, "t-ask").await.escalated, "not yet");
    // an hour and a minute later
    sqlx::query(
        "UPDATE mcp_tasks SET input_since = now() - interval '61 minutes' WHERE task_id = 't-ask'",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    make_due(&e.owner, "t-ask").await;
    p.tick().await.unwrap();
    let x = esc(&e, "t-ask").await;
    assert!(x.escalated && x.notify, "{x:?}");
    assert_eq!(x.reason.as_deref(), Some("no_answer"));
}

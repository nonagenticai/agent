//! B2 (Part B, real stack): an MCP task started in a conversation talks back to it. The relay asks the task's
//! question in the conversation, the user's reply goes to the task as the job's service actor (no model call), the
//! finished task's result is posted once, and a conversation closed by staff cancels its unfinished tasks. The MCP
//! server is a stub on loopback offering the tasks extension (the external boundary).
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_api::connectors::transport::Transport;
use agent_api::connectors::transport::mcp::McpTransport;
use agent_api::conversations::state::{
    CONVEYOR_PROJECT_ID, CONVEYOR_PROJECT_TASK, FR_PROJECT_ID, FR_PROJECT_TASK,
};
use agent_api::interaction::Scope as JobScope;
use agent_api::mcp_client::client::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
use agent_api::mcp_client::store::{NewTask, record_started};
use agent_api::mcp_client::tasks::{CONVEYOR_META, FR_META, TaskHandle};
use agent_api::netguard::Policy;
use agent_api::runtime::task_relay::{self, TaskRelay};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use uuid::Uuid;

use super::world::*;

const TASKS: &str = "io.modelcontextprotocol/tasks";
const QUESTION: &str = "t-q:01";
const PROJECT: &str = "91afede0-f7f7-5633-9522-050c0db00196";

type Seen = Arc<Mutex<Vec<Value>>>;

fn of(seen: &Seen, method: &str) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|b| b["method"] == method)
        .cloned()
        .collect()
}

/// A stub MCP server that acknowledges `tasks/update` and `tasks/cancel` and logs every request.
async fn stub() -> (Arc<HttpMcp>, Seen) {
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(move |body: String| {
            let s = s.clone();
            async move {
                let req: Value = serde_json::from_str(&body).unwrap();
                s.lock().unwrap().push(req.clone());
                let id = req["id"].clone();
                let ok = |r: Value| json!({"jsonrpc": "2.0", "id": id, "result": r});
                let reply = match req["method"].as_str().unwrap_or("") {
                    "server/discover" => ok(json!({"resultType": "complete", "supportedVersions": ["2026-07-28"], "ttlMs": 0, "cacheScope": "private",
                        "capabilities": {"tools": {}, "extensions": {TASKS: {}}},
                        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "stub", "version": "0"}}})),
                    "tasks/update" | "tasks/cancel" => ok(json!({"resultType": "complete"})),
                    _ => return StatusCode::ACCEPTED.into_response(),
                };
                axum::Json(reply).into_response()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let creds = TenantCredentials::new(FnSource(|_t: Uuid, _f: bool| async move {
        McpCredentials::new("lic", "bind", Instant::now() + Duration::from_secs(3600))
    }));
    (
        Arc::new(HttpMcp::new(format!("http://{addr}/mcp"), creds)),
        seen,
    )
}

/// The world with the MCP client on its transport, and a chat with one case.
async fn world(name: &str) -> (World, Seen, String, String) {
    world_with(name, say_only("unused")).await
}

/// [`world`] with a scripted model: the script answers the task answer (`[task task_answer]`); any other task reply
/// is the world's default JSON, which carries no `answer`.
async fn world_with(name: &str, script: Script) -> (World, Seen, String, String) {
    let mut w = World::build(name, script, false).await;
    let seen = wire_mcp(&mut w).await;
    let conv = w.start_chat(&w.f).await;
    let case = "CS-901".to_owned();
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev) \
         SELECT tenant_id, job_id, $2, conversation_id, channel, env, 0 FROM conversation WHERE conversation_id = $1",
    )
    .bind(&conv)
    .bind(&case)
    .execute(&w.e.owner)
    .await
    .unwrap();
    (w, seen, conv, case)
}

/// The world's connectors reach the stub MCP server.
async fn wire_mcp(w: &mut World) -> Seen {
    let (mcp, seen) = stub().await;
    w.e.state.connectors = Transport::with_mcp(
        Policy {
            allow_loopback: true,
            private_targets: vec![],
        },
        McpTransport::new(mcp, vec![]),
    );
    w.e.app = agent_api::router(w.e.state.clone());
    seen
}

/// A task of `case`, started as the job's service actor.
async fn started(w: &World, case: &str, id: &str) {
    let f = &w.f;
    let by = format!("job:{}", f.job.job_id);
    record_started(
        &w.e.state.db,
        &NewTask {
            tenant: f.tenant,
            job: f.job.job_id,
            operation: "refund.approve",
            idempotency_key: "idem-b2",
            case_id: Some(case),
            created_by: &by,
            origin: "platform",
            instance: None,
        },
        "approve_refund",
        &TaskHandle {
            task_id: id.into(),
            status: "working".into(),
            status_message: None,
            poll_interval_ms: Some(2000),
            ttl_ms: Some(86_400_000),
        },
    )
    .await
    .unwrap();
}

fn relay(w: &World) -> TaskRelay {
    TaskRelay {
        st: w.e.state.clone(),
        root: w.e.state.redis.clone().expect("REDIS_URL set"),
    }
}

async fn lines(w: &World, conv: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT text FROM conversation_message WHERE conversation_id = $1 AND role = 'agent' \
          ORDER BY at_ms, message_id",
    )
    .bind(conv)
    .fetch_all(&w.e.owner)
    .await
    .unwrap()
}

async fn lines_until(w: &World, conv: &str, n: usize) -> Vec<String> {
    until(&format!("{n} agent lines"), || async {
        let l = lines(w, conv).await;
        (l.len() >= n).then_some(l)
    })
    .await
}

#[tokio::test]
async fn a_tasks_question_is_asked_answered_as_the_job_and_its_result_posted_once() {
    let (w, seen, conv, case) = world("iam-b2-ask").await;
    let f = &w.f;
    started(&w, &case, "t-b2").await;
    sqlx::query(
        "UPDATE mcp_tasks SET status = 'input_required', last_detail = $2 WHERE task_id = $1",
    )
    .bind("t-b2")
    .bind(
        json!({"inputRequests": {QUESTION: {"method": "elicitation/create", "params": {
        "message": "Approve the refund of 40 EUR?",
        "requestedSchema": {"type": "object", "required": ["decision"], "properties": {
            "decision": {"type": "string", "enum": ["approved", "rejected"]}}}}}}}),
    )
    .execute(&w.e.owner)
    .await
    .unwrap();

    // asked once, however often the relay sweeps
    let r = relay(&w);
    r.sweep().await.unwrap();
    r.sweep().await.unwrap();
    let asked = lines(&w, &conv).await;
    assert_eq!(
        asked,
        ["Approve the refund of 40 EUR? (approved / rejected)"]
    );

    // a reply outside the enum is asked again, naming the options
    w.say(f, &conv, "maybe later").await;
    let l = lines_until(&w, &conv, 2).await;
    assert!(
        l[1].starts_with("Please answer one of: approved / rejected."),
        "{l:?}"
    );

    // the answer goes to the task as the job's service actor
    w.say(f, &conv, "Approved").await;
    let l = lines_until(&w, &conv, 3).await;
    assert_eq!(l[2], task_relay::ANSWERED_TEXT);
    let updates = of(&seen, "tasks/update");
    assert_eq!(updates.len(), 1, "{updates:?}");
    let p = &updates[0]["params"];
    assert_eq!(
        p["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("agent:{}", f.tenant), "job": f.job.job_id.to_string()})
    );
    assert_eq!(
        p["inputResponses"][QUESTION]["content"]["decision"], "approved",
        "{p}"
    );
    let by: String =
        sqlx::query_scalar("SELECT answered_by FROM mcp_task_answers WHERE task_id = 't-b2'")
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert_eq!(by, format!("job:{}", f.job.job_id));

    // the finished task's own words are posted once
    sqlx::query(
        "UPDATE mcp_tasks SET status = 'completed', terminal_at = now(), result = $2 WHERE task_id = $1",
    )
    .bind("t-b2")
    .bind(json!({"outcome": "completed", "ok": true, "result": "The refund of 40 EUR is approved."}))
    .execute(&w.e.owner)
    .await
    .unwrap();
    r.sweep().await.unwrap();
    r.sweep().await.unwrap();
    let l = lines(&w, &conv).await;
    assert_eq!(l.len(), 4, "{l:?}");
    assert_eq!(l[3], "The refund of 40 EUR is approved.");
    let delivered: bool =
        sqlx::query_scalar("SELECT delivered_at IS NOT NULL FROM mcp_tasks WHERE task_id = 't-b2'")
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert!(delivered);

    // the relay asked the model once (the agent's own attempt); its reply carried no answer, so the customer was
    // asked as before (owner 2026-10-31)
    let bodies = w.llm.bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(system_of(&bodies[0]).contains("[task task_answer]"));
}

/// A customer message the model is given (and the audit keeps), inserted directly: no turn runs for it.
async fn heard(w: &World, conv: &str, text: &str) -> Uuid {
    heard_as(w, conv, "customer", text).await
}

/// [`heard`] with the stored role: `caller` is what an API-key caller's message is stored as.
async fn heard_as(w: &World, conv: &str, role: &str, text: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, at_ms) \
         VALUES ($1, $2, $3, $5, $4, 1) RETURNING message_id",
    )
    .bind(w.f.tenant)
    .bind(w.f.job.job_id)
    .bind(conv)
    .bind(text)
    .bind(role)
    .fetch_one(&w.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_agent_answers_a_tasks_question_without_asking_the_customer() {
    let (w, seen, conv, case) = world_with(
        "iam-b2-agent-answer",
        say_only(r#"{"answer": {"decision": "Approved", "comment": "The rug arrived damaged."}}"#),
    )
    .await;
    let f = &w.f;
    started(&w, &case, "t-agent").await;
    waiting(&w, "t-agent").await;
    let told = heard(
        &w,
        &conv,
        "Please approve the refund for the rug that arrived damaged.",
    )
    .await;

    relay(&w).sweep().await.unwrap();

    // the customer is not asked anything
    assert!(lines(&w, &conv).await.is_empty());
    // fr gets the answer as the job's service actor
    let updates = of(&seen, "tasks/update");
    assert_eq!(updates.len(), 1, "{updates:?}");
    let p = &updates[0]["params"];
    assert_eq!(
        p["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("agent:{}", f.tenant), "job": f.job.job_id.to_string()})
    );
    assert_eq!(
        p["inputResponses"][QUESTION]["content"],
        json!({"decision": "approved", "comment": "The rug arrived damaged."}),
        "{p}"
    );
    // the job answered it, and the customer's message is kept behind it for audit
    let (by, source): (String, Vec<String>) = sqlx::query_as(
        "SELECT answered_by, source_messages FROM mcp_task_answers WHERE task_id = 't-agent'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(by, format!("job:{}", f.job.job_id));
    assert_eq!(source, [told.to_string()]);
    // one model call, and it was the task answer
    let bodies = w.llm.bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(system_of(&bodies[0]).contains("[task task_answer]"));
}

/// The task answer's ONE model call is metered as `task_answer`, attributed to the conversation's channel, with the
/// fake reply's token counts.
#[tokio::test]
async fn a_task_answer_is_metered() {
    let (w, _seen, conv, case) = world_with(
        "iam-b2-answer-usage",
        say_only(r#"{"answer": {"decision": "Approved", "comment": "The rug arrived damaged."}}"#),
    )
    .await;
    started(&w, &case, "t-usage").await;
    waiting(&w, "t-usage").await;

    relay(&w).sweep().await.unwrap();

    let rows: Vec<(String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT use, source, actor, tokens_in, tokens_out FROM usage_event \
          WHERE job_id = $1 AND use = 'task_answer'",
    )
    .bind(w.f.job.job_id)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (use_, source, actor, tokens_in, tokens_out) = rows.into_iter().next().unwrap();
    assert_eq!(use_, agent_api::runtime::task_answer::USAGE_LABEL);
    // the conversation's channel decides the attribution: the staff test chat is `staff_test` by its member
    let (channel, initiator): (String, Option<String>) = sqlx::query_as(
        "SELECT channel, initiator_sub FROM conversation WHERE conversation_id = $1",
    )
    .bind(&conv)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(channel, "staff");
    assert_eq!(source, "staff_test");
    assert_eq!(Some(actor), initiator);
    // the fake model's `usage` block
    assert_eq!((tokens_in, tokens_out), (9, 5));
}

/// The task `id` waits on QUESTION with fr's real approval schema: a required decision and the answer text fields
/// (`comment` for an approve, `reason` for a reject).
async fn waiting_for_two(w: &World, id: &str) {
    sqlx::query(
        "UPDATE mcp_tasks SET status = 'input_required', input_since = now(), last_detail = $2 WHERE task_id = $1",
    )
    .bind(id)
    .bind(
        json!({"inputRequests": {QUESTION: {"method": "elicitation/create", "params": {
        "message": "Approve the refund of 40 EUR?",
        "requestedSchema": {"type": "object", "required": ["decision"], "properties": {
            "decision": {"type": "string", "enum": ["approved", "rejected"]},
            "reason": {"type": "string", "description": "Why"},
            "comment": {"type": "string"}}}}}}}),
    )
    .execute(&w.e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_partial_answer_keeps_the_agents_values_and_asks_the_customer_for_the_rest() {
    let (w, seen, conv, case) = world_with(
        "iam-b2-agent-partial",
        say_only(r#"{"answer": {"reason": "The rug arrived damaged."}}"#),
    )
    .await;
    let f = &w.f;
    started(&w, &case, "t-partial").await;
    waiting_for_two(&w, "t-partial").await;

    // the agent leaves the decision out (a choice only the customer can make): only that is asked
    relay(&w).sweep().await.unwrap();
    assert_eq!(
        lines(&w, &conv).await,
        ["Approve the refund of 40 EUR?\ndecision (approved / rejected)"]
    );
    assert!(of(&seen, "tasks/update").is_empty());

    // the customer's answer carries the agent's reason with it; fr reads an approve's text from `comment`, so the
    // relay asks for that note before anything is sent (C0)
    w.say(f, &conv, "Approved").await;
    let l = lines_until(&w, &conv, 2).await;
    assert_eq!(l[1], "What note should go with it?");
    w.say(f, &conv, "The rug arrived damaged.").await;
    let l = lines_until(&w, &conv, 3).await;
    assert_eq!(l[2], task_relay::ANSWERED_TEXT);
    let updates = of(&seen, "tasks/update");
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(
        updates[0]["params"]["inputResponses"][QUESTION]["content"],
        json!({"decision": "approved", "reason": "The rug arrived damaged.",
               "comment": "The rug arrived damaged."}),
        "{:?}",
        updates[0]["params"]
    );
}

#[tokio::test]
async fn without_a_model_the_customer_is_asked_as_before() {
    let (w, seen, conv, case) = world("iam-b2-no-model").await;
    let f = &w.f;
    sqlx::query("UPDATE tenants SET platform_owned = false WHERE id = $1")
        .bind(f.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    started(&w, &case, "t-no-model").await;
    waiting(&w, "t-no-model").await;

    relay(&w).sweep().await.unwrap();

    assert_eq!(
        lines(&w, &conv).await,
        ["Approve the refund of 40 EUR?\ndecision (approved / rejected)"]
    );
    assert!(of(&seen, "tasks/update").is_empty());
    assert!(w.llm.bodies().is_empty(), "{:?}", w.llm.bodies());
}

/// The agent is not done with the question while another relay holds its claim: the customer is not asked until the
/// claim has run out (ten minutes) and this relay had its own turn.
#[tokio::test]
async fn a_question_another_relay_is_answering_is_not_asked_of_the_customer() {
    let (w, _seen, conv, case) = world("iam-b2-claimed").await;
    let f = &w.f;
    sqlx::query("UPDATE tenants SET platform_owned = false WHERE id = $1")
        .bind(f.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    started(&w, &case, "t-claimed").await;
    waiting(&w, "t-claimed").await;
    let claim = |age: &'static str| {
        sqlx::query(
            "UPDATE mcp_tasks SET agent_attempted_at = now() - $2::interval, agent_attempted_question = $3 \
              WHERE task_id = $1",
        )
        .bind("t-claimed")
        .bind(age)
        .bind(QUESTION)
        .execute(&w.e.owner)
    };
    claim("0 seconds").await.unwrap();

    relay(&w).sweep().await.unwrap();
    assert!(lines(&w, &conv).await.is_empty());

    claim("11 minutes").await.unwrap();
    relay(&w).sweep().await.unwrap();
    assert_eq!(
        lines(&w, &conv).await,
        ["Approve the refund of 40 EUR?\ndecision (approved / rejected)"]
    );
}

/// One model call per sweep: the second task waiting in the same sweep is left for the next sweep, never asked of
/// the customer before the agent had its turn on it.
#[tokio::test]
async fn a_second_task_in_one_sweep_waits_for_the_agents_turn() {
    let (w, _seen, conv, case) = world("iam-b2-budget").await;
    let f = &w.f;
    sqlx::query("UPDATE tenants SET platform_owned = false WHERE id = $1")
        .bind(f.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    for id in ["t-first", "t-second"] {
        started(&w, &case, id).await;
        waiting(&w, id).await;
    }
    let done = || {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM mcp_tasks WHERE agent_done_question IS NOT NULL",
        )
        .fetch_one(&w.e.owner)
    };

    let r = relay(&w);
    r.sweep().await.unwrap();
    assert_eq!(done().await.unwrap(), 1);
    assert_eq!(lines(&w, &conv).await.len(), 1);

    r.sweep().await.unwrap();
    assert_eq!(done().await.unwrap(), 2);
    assert_eq!(lines(&w, &conv).await.len(), 2);
}

#[tokio::test]
async fn a_conversation_closed_by_staff_cancels_its_unfinished_tasks() {
    let (w, seen, conv, case) = world("iam-b2-cancel").await;
    let f = &w.f;
    started(&w, &case, "t-open").await;
    started(&w, &case, "t-finished").await;
    sqlx::query("UPDATE mcp_tasks SET status = 'completed', terminal_at = now() WHERE task_id = 't-finished'")
        .execute(&w.e.owner)
        .await
        .unwrap();
    let scope = JobScope {
        tenant: f.tenant,
        job_id: f.job.job_id,
        job_key: f.job.job_key.clone(),
    };
    task_relay::cancel_open(&w.e.state, &scope, &conv).await;
    let cancels = of(&seen, "tasks/cancel");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["params"]["taskId"], "t-open");
    let (req, sent): (bool, bool) = sqlx::query_as(
        "SELECT cancel_requested, cancel_sent FROM mcp_tasks WHERE task_id = 't-open'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(req && sent);
}

/// The task `id` waits on QUESTION (fr's real approval schema: decision, reason, comment).
async fn waiting(w: &World, id: &str) {
    sqlx::query(
        "UPDATE mcp_tasks SET status = 'input_required', input_since = now(), last_detail = $2 WHERE task_id = $1",
    )
    .bind(id)
    .bind(
        json!({"inputRequests": {QUESTION: {"method": "elicitation/create", "params": {
        "message": "Approve the refund of 40 EUR?",
        "requestedSchema": {"type": "object", "required": ["decision"], "properties": {
            "decision": {"type": "string", "enum": ["approved", "rejected"]},
            "reason": {"type": "string"},
            "comment": {"type": "string"}}}}}}}),
    )
    .execute(&w.e.owner)
    .await
    .unwrap();
}

/// The task `id` waits on a question that offers only the decision (no answer text to send with it).
async fn waiting_decision_only(w: &World, id: &str) {
    sqlx::query(
        "UPDATE mcp_tasks SET status = 'input_required', input_since = now(), last_detail = $2 WHERE task_id = $1",
    )
    .bind(id)
    .bind(
        json!({"inputRequests": {QUESTION: {"method": "elicitation/create", "params": {
        "message": "Approve the refund of 40 EUR?",
        "requestedSchema": {"type": "object", "required": ["decision"], "properties": {
            "decision": {"type": "string", "enum": ["approved", "rejected"]}}}}}}}),
    )
    .execute(&w.e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn an_escalated_task_is_not_asked_and_the_customer_is_told_once() {
    let (w, _seen, conv, case) = world("iam-b2-escalated").await;
    started(&w, &case, "t-esc").await;
    waiting(&w, "t-esc").await;
    let mut c = w.e.owner.acquire().await.unwrap();
    assert!(
        agent_api::mcp_client::store::escalate(&mut c, "t-esc", "refused:decide_in_fr", true)
            .await
            .unwrap()
    );
    let r = relay(&w);
    r.sweep().await.unwrap();
    r.sweep().await.unwrap();
    assert_eq!(lines(&w, &conv).await, [task_relay::ESCALATED_TEXT]);
}

#[tokio::test]
async fn a_handover_hands_the_conversations_open_tasks_to_staff_silently() {
    let (w, _seen, conv, case) = world("iam-b2-handover").await;
    started(&w, &case, "t-ho").await;
    waiting(&w, "t-ho").await;
    let mut c = w.e.owner.acquire().await.unwrap();
    let n = agent_api::mcp_client::store::escalate_conversation(&mut c, &conv, "handover")
        .await
        .unwrap();
    assert_eq!(n, 1);
    let (reason, told): (String, bool) = sqlx::query_as(
        "SELECT escalation_reason, escalation_notified_at IS NOT NULL FROM mcp_tasks WHERE task_id = 't-ho'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!((reason.as_str(), told), ("handover", true));
    // the handover told the customer; the relay neither asks nor adds a line
    relay(&w).sweep().await.unwrap();
    assert!(lines(&w, &conv).await.is_empty());
}

/// The task `id` waits on QUESTION with one required field, an id.
async fn waiting_for_an_id(w: &World, id: &str) {
    sqlx::query(
        "UPDATE mcp_tasks SET status = 'input_required', input_since = now(), last_detail = $2 WHERE task_id = $1",
    )
    .bind(id)
    .bind(
        json!({"inputRequests": {QUESTION: {"method": "elicitation/create", "params": {
        "message": "Which project is this for?",
        "requestedSchema": {"type": "object", "required": ["project_id"], "properties": {
            "project_id": {"type": "string"}}}}}}}),
    )
    .execute(&w.e.owner)
    .await
    .unwrap();
}

/// Owner 2026-11-01: the never-ask-for-ids rule governs our demos, not the platform. A field the model leaves out —
/// an id included — is asked of the requester, and their reply answers the task.
#[tokio::test]
async fn a_field_the_agent_could_not_fill_is_asked_of_the_requester() {
    let (w, seen, conv, case) = world_with("iam-b2-id-field", say_only(r#"{"answer": {}}"#)).await;
    started(&w, &case, "t-id").await;
    waiting_for_an_id(&w, "t-id").await;

    let r = relay(&w);
    r.sweep().await.unwrap();
    r.sweep().await.unwrap();

    // the agent had its one turn, nothing was sent to fr, and the requester is asked for the field
    assert_eq!(w.llm.bodies().len(), 1, "{:?}", w.llm.bodies());
    assert!(of(&seen, "tasks/update").is_empty());
    assert_eq!(lines(&w, &conv).await, ["Which project is this for?"]);
    w.say(&w.f, &conv, "PRJ-42").await;
    let l = lines_until(&w, &conv, 2).await;
    assert_eq!(l[1], task_relay::ANSWERED_TEXT);
    let updates = of(&seen, "tasks/update");
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(
        updates[0]["params"]["inputResponses"][QUESTION]["content"],
        json!({"project_id": "PRJ-42"})
    );
}

/// C0 (owner 2026-11-01): fr requires answer text with a decision. A model answer that approves or rejects without
/// it is not sent (it would spend the approval and leave the card blocked); the requester is asked.
#[tokio::test]
async fn a_decision_without_its_answer_text_is_not_sent_and_the_requester_is_asked() {
    let (w, seen, conv, case) = world_with(
        "iam-b2-decision-no-text",
        say_only(r#"{"answer": {"decision": "approved"}}"#),
    )
    .await;
    started(&w, &case, "t-c0").await;
    waiting(&w, "t-c0").await;

    let r = relay(&w);
    r.sweep().await.unwrap();
    r.sweep().await.unwrap();

    // nothing reached fr; the requester is asked for the decision (the text follows their own answer)
    assert!(of(&seen, "tasks/update").is_empty());
    assert_eq!(
        lines(&w, &conv).await,
        ["Approve the refund of 40 EUR?\ndecision (approved / rejected)"]
    );
    w.say(&w.f, &conv, "Approved").await;
    let l = lines_until(&w, &conv, 2).await;
    assert_eq!(l[1], "What note should go with it?");
    w.say(&w.f, &conv, "The rug arrived damaged.").await;
    let l = lines_until(&w, &conv, 3).await;
    assert_eq!(l[2], task_relay::ANSWERED_TEXT);
    let updates = of(&seen, "tasks/update");
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(
        updates[0]["params"]["inputResponses"][QUESTION]["content"],
        json!({"decision": "approved", "comment": "The rug arrived damaged."})
    );
}

/// A question that offers no answer text at all can never be decided by the model: the requester decides.
#[tokio::test]
async fn a_decision_only_question_is_never_answered_for_the_requester() {
    let (w, seen, conv, case) = world_with(
        "iam-b2-decision-only",
        say_only(r#"{"answer": {"decision": "approved"}}"#),
    )
    .await;
    started(&w, &case, "t-only").await;
    waiting_decision_only(&w, "t-only").await;

    relay(&w).sweep().await.unwrap();

    assert!(of(&seen, "tasks/update").is_empty());
    assert_eq!(
        lines(&w, &conv).await,
        ["Approve the refund of 40 EUR? (approved / rejected)"]
    );
}

/// An API-key caller's messages are stored as `caller`: they are the requester's words, so the model reads them and
/// the audit keeps them behind the job's answer.
#[tokio::test]
async fn a_callers_words_are_the_source_of_the_agents_answer() {
    let (w, seen, conv, case) = world_with(
        "iam-b2-caller-source",
        say_only(r#"{"answer": {"decision": "approved", "comment": "Approved as agreed."}}"#),
    )
    .await;
    started(&w, &case, "t-caller").await;
    waiting(&w, "t-caller").await;
    let told = heard_as(&w, &conv, "caller", "Card card-42: approve the refund.").await;

    relay(&w).sweep().await.unwrap();

    assert_eq!(of(&seen, "tasks/update").len(), 1);
    let source: Vec<String> = sqlx::query_scalar(
        "SELECT source_messages FROM mcp_task_answers WHERE task_id = 't-caller'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(source, [told.to_string()]);
    // the words reach the model as data, inside the conversation block
    let bodies = w.llm.bodies();
    let user = bodies[0]["messages"].to_string();
    assert!(user.contains("caller: Card card-42"), "{user}");
    assert!(user.contains("<conversation>"), "{user}");
}

/// An API-key caller's async request leaves its callback URL on the conversation; a task started in that thread then
/// reaches the caller there: the question, the escalation notice and the result, each signed, none twice.
#[tokio::test]
async fn an_api_callers_task_messages_reach_its_callback() {
    let script = tool_then_say(
        "case_comment",
        json!({"case_id": "card-42", "comment": "Refund 40 EUR"}),
        ANSWER,
    );
    let (mut w, key) = World::live("iam-b2-callback", script, json!({})).await;
    wire_mcp(&mut w).await;
    let f = &w.f;
    let secret = w.webhook_secret(f).await;
    let mut rx = Receiver::start().await;

    let (s, b) = w
        .keyed(
            f,
            &key,
            json!({"input": "Card card-42: refund 40 EUR", "callback_url": rx.url,
                   "conversation": {"id": "card-42"}}),
        )
        .await;
    assert_eq!(s, 202, "{b}");
    let (h, body) = rx.next().await;
    verify_signature(&secret, &h, &body);
    let answer: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["object"], "interaction", "{answer}");

    let conv: String =
        sqlx::query_scalar("SELECT conversation_id FROM conversation WHERE job_id = $1")
            .bind(f.job.job_id)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    let state = w.state(f, &conv).await;
    let url = state.iter().find(|s| s.0 == "callback_url");
    assert_eq!(url.and_then(|s| s.2.as_deref()), Some(rx.url.as_str()));
    let case: String = sqlx::query_scalar("SELECT case_id FROM agent_case WHERE job_id = $1")
        .bind(f.job.job_id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();

    // the question: the agent could not answer it, so the caller is asked, at its callback
    started(&w, &case, "t-cb").await;
    waiting(&w, "t-cb").await;
    let r = relay(&w);
    r.sweep().await.unwrap();
    r.sweep().await.unwrap();
    let (h, body) = rx.next().await;
    verify_signature(&secret, &h, &body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "interaction.message", "{v}");
    assert_eq!(v["kind"], "question", "{v}");
    assert_eq!(v["conversation"]["id"], "card-42", "{v}");
    assert_eq!(
        v["message"]["text"], "Approve the refund of 40 EUR?\ndecision (approved / rejected)",
        "{v}"
    );
    assert!(v["message"]["id"].is_string(), "{v}");
    rx.quiet(300).await;

    // staff take the task over: the caller is told once
    let mut c = w.e.owner.acquire().await.unwrap();
    assert!(
        agent_api::mcp_client::store::escalate(&mut c, "t-cb", "refused:decide_in_fr", true)
            .await
            .unwrap()
    );
    r.sweep().await.unwrap();
    r.sweep().await.unwrap();
    let (h, body) = rx.next().await;
    verify_signature(&secret, &h, &body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["kind"], "escalated", "{v}");
    assert_eq!(v["message"]["text"], task_relay::ESCALATED_TEXT, "{v}");
    rx.quiet(300).await;
}

/// The task `id`'s `tasks/get` detail carries fr's `_meta`: the project id, the activity log and the signals.
async fn meta(w: &World, id: &str, meta: Value) {
    sqlx::query("UPDATE mcp_tasks SET last_detail = $2 WHERE task_id = $1")
        .bind(id)
        .bind(json!({"_meta": {(FR_META): meta}}))
        .execute(&w.e.owner)
        .await
        .unwrap();
}

/// The task `id`'s `tasks/get` detail carries the conveyor's `_meta`: project, run and events, under the same
/// channel key fr uses for its own block, with no signals.
async fn conveyor_meta(w: &World, id: &str, block: Value) {
    sqlx::query("UPDATE mcp_tasks SET last_detail = $2 WHERE task_id = $1")
        .bind(id)
        .bind(json!({"_meta": {(CONVEYOR_META): block}}))
        .execute(&w.e.owner)
        .await
        .unwrap();
}

/// Make the sweep consider `id` due again although nothing changed: what the relay dedupes by itself is then tested,
/// not the due query.
async fn due_again(w: &World, id: &str) {
    sqlx::query("UPDATE mcp_tasks SET last_relayed_status = NULL WHERE task_id = $1")
        .bind(id)
        .execute(&w.e.owner)
        .await
        .unwrap();
}

/// Whether the relay's due query returns `id`: the sweep's due set as the database sees it.
async fn is_due(w: &World, id: &str) -> bool {
    sqlx::query_scalar::<_, String>(
        "SELECT task_id FROM agent_mcp_task_relay_due(50) WHERE task_id = $1",
    )
    .bind(id)
    .fetch_optional(&w.e.owner)
    .await
    .unwrap()
    .is_some()
}

/// Whether the task is escalated and why.
async fn escalation(w: &World, id: &str) -> (bool, Option<String>) {
    sqlx::query_as::<_, (bool, Option<String>)>(
        "SELECT escalated_at IS NOT NULL, escalation_reason FROM mcp_tasks WHERE task_id = $1",
    )
    .bind(id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap()
}

/// The server's fr `_meta` is relayed while the task is live: the one project-started line, then one message per NEW
/// activity entry (strictly by seq). A later sweep, due again, says nothing twice.
#[tokio::test]
async fn a_tasks_meta_project_and_its_activity_are_relayed_once() {
    let (w, _seen, conv, case) = world("iam-b2-progress").await;
    started(&w, &case, "t-progress").await;
    meta(
        &w,
        "t-progress",
        json!({"project_id": PROJECT, "signals": [], "activity": [
            {"seq": 1, "at": "2026-11-06T10:00:00Z", "kind": "started",
             "text": "The project started.", "severity": "info"},
            {"seq": 2, "at": "2026-11-06T10:00:05Z", "kind": "phase",
             "text": "Writing the brief.", "severity": "info"}]}),
    )
    .await;

    let r = relay(&w);
    r.sweep().await.unwrap();
    // The server's own `started` entry IS the start line: the fixed text would be a second one.
    assert_eq!(
        lines(&w, &conv).await,
        ["The project started.", "Writing the brief."]
    );

    due_again(&w, "t-progress").await;
    r.sweep().await.unwrap();
    assert_eq!(lines(&w, &conv).await.len(), 2);
}

/// The project id reaches the conversation's state once, under the platform's own key, and the project-started line
/// is posted once.
#[tokio::test]
async fn the_fr_project_id_is_written_to_the_conversation_state_once() {
    let (w, _seen, conv, case) = world("iam-b2-project-state").await;
    started(&w, &case, "t-state").await;
    meta(
        &w,
        "t-state",
        json!({"project_id": PROJECT, "signals": [], "activity": []}),
    )
    .await;

    let r = relay(&w);
    r.sweep().await.unwrap();
    due_again(&w, "t-state").await;
    r.sweep().await.unwrap();

    let kept: Vec<_> = w
        .state(&w.f, &conv)
        .await
        .into_iter()
        .filter(|s| s.0 == FR_PROJECT_ID)
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].1, "value");
    assert_eq!(kept[0].2.as_deref(), Some(PROJECT));
    assert_eq!(lines(&w, &conv).await, [task_relay::PROJECT_STARTED_TEXT]);
}

/// The newest STARTED task wins `fr_project_id`: an older task's project seen first is superseded by a newer task's
/// project, and the older task relayed last does not take it back. `fr_project_task` names the task behind the kept
/// project; the started line is posted on first sight only.
#[tokio::test]
async fn the_newest_started_project_wins_the_state_key() {
    let (w, _seen, conv, case) = world("iam-b2-project-newest").await;
    let newer = "91afede0-f7f7-5633-9522-050c0db00197";
    let older = PROJECT;
    started(&w, &case, "t-newest").await;
    started(&w, &case, "t-oldest").await;
    // explicit start times: both tasks exist before either is relayed
    sqlx::query(
        "UPDATE mcp_tasks SET created_at = now() - interval '1 hour' WHERE task_id = 't-oldest'",
    )
    .execute(&w.e.owner)
    .await
    .unwrap();
    meta(
        &w,
        "t-newest",
        json!({"project_id": newer, "signals": [], "activity": []}),
    )
    .await;
    meta(
        &w,
        "t-oldest",
        json!({"project_id": older, "signals": [], "activity": []}),
    )
    .await;

    // the older task's project reaches the conversation first (first sight: the started line), the newer task then
    // supersedes it, and the older one swept last does not take it back
    let r = relay(&w);
    let mut budget = true;
    r.relay(w.f.tenant, w.f.job.job_id, "t-oldest", &mut budget)
        .await
        .unwrap();
    r.relay(w.f.tenant, w.f.job.job_id, "t-newest", &mut budget)
        .await
        .unwrap();
    r.relay(w.f.tenant, w.f.job.job_id, "t-oldest", &mut budget)
        .await
        .unwrap();

    let kept: Vec<_> = w
        .state(&w.f, &conv)
        .await
        .into_iter()
        .filter(|s| s.0 == FR_PROJECT_ID || s.0 == FR_PROJECT_TASK)
        .collect();
    assert_eq!(kept.len(), 2, "{kept:?}");
    let value = |key: &str| kept.iter().find(|s| s.0 == key).and_then(|s| s.2.clone());
    assert_eq!(value(FR_PROJECT_ID).as_deref(), Some(newer), "{kept:?}");
    assert_eq!(
        value(FR_PROJECT_TASK).as_deref(),
        Some("t-newest"),
        "{kept:?}"
    );
    // the started line is posted on first sight, and not again for the older task
    assert_eq!(lines(&w, &conv).await, [task_relay::PROJECT_STARTED_TEXT]);
}

/// A failed fr project reaches the agent as a COMPLETED task whose result is an error, and the server reports
/// `project_failed` (critical) only on that final `_meta`. The sweep must still escalate it — otherwise the
/// requester is told "someone will look into it" and nobody is ever handed the task.
#[tokio::test]
async fn a_failed_project_escalates_even_though_its_task_is_completed() {
    let (w, _seen, conv, case) = world("iam-b2-terminal-signal").await;
    started(&w, &case, "t-failed").await;
    meta(
        &w,
        "t-failed",
        json!({"project_id": "p-1", "activity": [], "signals": [
            {"code": "project_failed", "severity": "critical",
             "detail": "the fr run failed"}]}),
    )
    .await;
    // The server's mapping: a failed external run is `completed` + `ok: false` on the wire.
    sqlx::query(
        "UPDATE mcp_tasks SET status='completed', terminal_at=now(), result=$2 WHERE task_id=$1",
    )
    .bind("t-failed")
    .bind(json!({"outcome": "completed", "ok": false, "error": "failed", "retryable": false}))
    .execute(&w.e.owner)
    .await
    .unwrap();

    let r = relay(&w);
    r.sweep().await.unwrap();
    assert_eq!(
        escalation(&w, "t-failed").await,
        (true, Some("fr:critical:project_failed".to_owned()))
    );
    let told = lines(&w, &conv).await;
    assert!(
        told.iter().any(|line| line.contains("critical")),
        "the requester is not told about the severity: {told:?}"
    );
    assert!(told.iter().any(|line| line == task_relay::FAILED_TEXT));
}

/// A `critical` signal goes to staff through the existing escalation path and tells the requester once, naming the
/// severity. A later sweep that carries the same signal neither escalates again nor repeats the notice.
#[tokio::test]
async fn a_critical_signal_escalates_the_task_once_and_tells_the_requester_once() {
    let (w, _seen, conv, case) = world("iam-b2-signal").await;
    started(&w, &case, "t-signal").await;
    meta(
        &w,
        "t-signal",
        json!({"project_id": null, "activity": [], "signals": [
            {"code": "delivery_missed", "severity": "critical",
             "detail": "The delivery deadline passed."}]}),
    )
    .await;

    let r = relay(&w);
    r.sweep().await.unwrap();
    assert_eq!(
        escalation(&w, "t-signal").await,
        (true, Some("fr:critical:delivery_missed".to_owned()))
    );
    let told = lines(&w, &conv).await;
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("critical"), "{told:?}");
    assert!(
        told[0].contains("The delivery deadline passed."),
        "{told:?}"
    );

    due_again(&w, "t-signal").await;
    r.sweep().await.unwrap();
    assert_eq!(lines(&w, &conv).await, told);
    assert_eq!(
        escalation(&w, "t-signal").await,
        (true, Some("fr:critical:delivery_missed".to_owned()))
    );
}

/// A strictly worse signal upgrades the escalation's reason and tells the requester about the worse severity. A
/// repeat, or a signal weaker than what the task already holds, adds nothing.
#[tokio::test]
async fn a_worse_signal_upgrades_the_notice_and_a_weaker_one_does_not() {
    let (w, _seen, conv, case) = world("iam-b2-signal-upgrade").await;
    started(&w, &case, "t-upgrade").await;
    let signal = |severity: &str, code: &str, detail: &str| {
        json!({"project_id": null, "activity": [], "signals": [
            {"code": code, "severity": severity, "detail": detail}]})
    };
    meta(
        &w,
        "t-upgrade",
        signal("high", "delivery_late", "The delivery is late."),
    )
    .await;

    let r = relay(&w);
    r.sweep().await.unwrap();
    assert_eq!(
        escalation(&w, "t-upgrade").await,
        (true, Some("fr:high:delivery_late".to_owned()))
    );
    let told = lines(&w, &conv).await;
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("high"), "{told:?}");

    // critical is strictly worse: the reason and the notice both move
    meta(
        &w,
        "t-upgrade",
        signal("critical", "project_failed", "The fr run failed."),
    )
    .await;
    due_again(&w, "t-upgrade").await;
    r.sweep().await.unwrap();
    assert_eq!(
        escalation(&w, "t-upgrade").await,
        (true, Some("fr:critical:project_failed".to_owned()))
    );
    let told = lines(&w, &conv).await;
    assert_eq!(told.len(), 2, "{told:?}");
    assert!(told[1].contains("critical"), "{told:?}");

    // a high signal again is weaker than what the task holds: neither escalated nor told again
    meta(
        &w,
        "t-upgrade",
        signal("high", "delivery_late", "The delivery is late."),
    )
    .await;
    due_again(&w, "t-upgrade").await;
    r.sweep().await.unwrap();
    assert_eq!(lines(&w, &conv).await, told);
    assert_eq!(
        escalation(&w, "t-upgrade").await,
        (true, Some("fr:critical:project_failed".to_owned()))
    );
}

/// A notice whose post fails (here: the case's conversation row does not exist yet, so the agent line cannot be
/// stored) leaves the task escalated with its notice claim released; once the conversation exists, the next sweep
/// tells the requester exactly once.
#[tokio::test]
async fn a_failed_escalation_notice_is_retried_once_more() {
    let (w, _seen, conv, _case) = world("iam-b2-notice-retry").await;
    let ghost = "conv-not-yet";
    let ghost_case = "CS-902";
    // the task's case lives in a conversation that does not exist yet: post() cannot store its line
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev) \
         SELECT tenant_id, job_id, $2, $3, channel, env, 0 FROM conversation WHERE conversation_id = $1",
    )
    .bind(&conv)
    .bind(ghost_case)
    .bind(ghost)
    .execute(&w.e.owner)
    .await
    .unwrap();
    started(&w, ghost_case, "t-retry").await;
    meta(
        &w,
        "t-retry",
        json!({"project_id": null, "activity": [], "signals": [
            {"code": "delivery_missed", "severity": "critical",
             "detail": "The delivery deadline passed."}]}),
    )
    .await;

    let r = relay(&w);
    r.sweep().await.unwrap();
    // the escalation stands, but its notice was never delivered: the claim is released for the next sweep
    assert_eq!(
        escalation(&w, "t-retry").await,
        (true, Some("fr:critical:delivery_missed".to_owned()))
    );
    let (unclaimed,): (bool,) = sqlx::query_as(
        "SELECT escalation_notified_at IS NULL FROM mcp_tasks WHERE task_id = 't-retry'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(unclaimed, "the failed post leaves the notice unclaimed");

    // the conversation exists now: the next sweep tells the requester, once
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) \
         VALUES ($1, $2, $3, 'test', 'staff')",
    )
    .bind(w.f.tenant)
    .bind(w.f.job.job_id)
    .bind(ghost)
    .execute(&w.e.owner)
    .await
    .unwrap();
    due_again(&w, "t-retry").await;
    r.sweep().await.unwrap();
    let told = lines(&w, ghost).await;
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("critical"), "{told:?}");
    assert!(
        told[0].contains("The delivery deadline passed."),
        "{told:?}"
    );
    let (notified,): (bool,) = sqlx::query_as(
        "SELECT escalation_notified_at IS NOT NULL FROM mcp_tasks WHERE task_id = 't-retry'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(notified);

    // and no later sweep repeats it
    due_again(&w, "t-retry").await;
    r.sweep().await.unwrap();
    assert_eq!(lines(&w, ghost).await, told);
}

/// Two sweeps running at once tell the requester about one escalation exactly once: the notice claim is the
/// once-only guard. (It passes on the pre-claim code too — `escalate`'s UPDATE was already atomic — so it guards the
/// claim protocol itself, not the retry or the upgrade.)
#[tokio::test]
async fn concurrent_sweeps_tell_the_requester_once() {
    let (w, _seen, conv, case) = world("iam-b2-signal-concurrent").await;
    started(&w, &case, "t-concurrent").await;
    meta(
        &w,
        "t-concurrent",
        json!({"project_id": null, "activity": [], "signals": [
            {"code": "project_failed", "severity": "critical", "detail": "The fr run failed."}]}),
    )
    .await;

    let r = relay(&w);
    let (a, b) = tokio::join!(r.sweep(), r.sweep());
    a.unwrap();
    b.unwrap();

    let told = lines(&w, &conv).await;
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("critical"), "{told:?}");
    let (notified,): (bool,) = sqlx::query_as(
        "SELECT escalation_notified_at IS NOT NULL FROM mcp_tasks WHERE task_id = 't-concurrent'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(notified);
}

/// A conversation no async request ever named a callback for keeps the message in its history only.
#[tokio::test]
async fn a_conversation_without_a_callback_delivers_nothing_to_one() {
    let (mut w, key) = World::live("iam-b2-no-callback", say_only(ANSWER), json!({})).await;
    wire_mcp(&mut w).await;
    let f = &w.f;
    let (s, b) = w
        .keyed(
            f,
            &key,
            json!({"input": "Card card-43: hello", "conversation": {"id": "card-43"}}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    let n = w
        .count(
            "SELECT count(*) FROM interaction_deliveries WHERE job_id = $1",
            f,
        )
        .await;
    let conv: String =
        sqlx::query_scalar("SELECT conversation_id FROM conversation WHERE job_id = $1")
            .bind(f.job.job_id)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert!(
        w.state(f, &conv)
            .await
            .iter()
            .all(|s| s.0 != "callback_url")
    );
    assert_eq!(n, 0);
}

/// An ESCALATED task that is WAITING (staff own it) whose notice post fails: the claim is released, and once the
/// conversation exists the next sweep tells the requester once. This is the `input_required` copy of the release —
/// the `signals` copy has its own test above; removing either one leaves its branch silent for good.
#[tokio::test]
async fn an_escalated_waiting_task_retries_its_notice_after_a_failed_post() {
    let (w, _seen, conv, _case) = world("iam-b2-wait-retry").await;
    let ghost = "conv-wait-not-yet";
    let ghost_case = "CS-903";
    // the task's case lives in a conversation that does not exist yet: post() cannot store its line
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev) \
         SELECT tenant_id, job_id, $2, $3, channel, env, 0 FROM conversation WHERE conversation_id = $1",
    )
    .bind(&conv)
    .bind(ghost_case)
    .bind(ghost)
    .execute(&w.e.owner)
    .await
    .unwrap();
    started(&w, ghost_case, "t-wait-retry").await;
    waiting(&w, "t-wait-retry").await;
    // staff took it: escalated, and the requester has not been told yet
    sqlx::query(
        "UPDATE mcp_tasks SET escalated_at = now(), escalation_reason = 'staff:handover', \
         escalation_notified_at = NULL WHERE task_id = 't-wait-retry'",
    )
    .execute(&w.e.owner)
    .await
    .unwrap();

    let r = relay(&w);
    r.sweep().await.unwrap();
    // the escalation stands, the notice does not: the failed post released the claim
    assert_eq!(
        escalation(&w, "t-wait-retry").await,
        (true, Some("staff:handover".to_owned()))
    );
    let (unclaimed,): (bool,) = sqlx::query_as(
        "SELECT escalation_notified_at IS NULL FROM mcp_tasks WHERE task_id = 't-wait-retry'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(unclaimed, "the failed post leaves the notice unclaimed");

    // the conversation exists now: the next sweep tells the requester, once
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) \
         VALUES ($1, $2, $3, 'test', 'staff')",
    )
    .bind(w.f.tenant)
    .bind(w.f.job.job_id)
    .bind(ghost)
    .execute(&w.e.owner)
    .await
    .unwrap();
    due_again(&w, "t-wait-retry").await;
    r.sweep().await.unwrap();
    let told = lines(&w, ghost).await;
    assert_eq!(told, [task_relay::ESCALATED_TEXT], "{told:?}");

    // and no later sweep repeats it
    due_again(&w, "t-wait-retry").await;
    r.sweep().await.unwrap();
    assert_eq!(lines(&w, ghost).await, told);
}

/// The project compare-and-write runs under the conversation's advisory lock: a holder of `fr-project:<conv>` makes
/// the relay wait, and nothing is written until the lock is released. Without the lock the relay writes at once and
/// the first assertion here fails — this is the control for the lock, not for the ordering rule.
#[tokio::test]
async fn the_project_write_waits_for_the_conversations_lock() {
    let (w, _seen, conv, case) = world("iam-b2-project-lock").await;
    started(&w, &case, "t-lock").await;
    meta(
        &w,
        "t-lock",
        json!({"project_id": PROJECT, "signals": [], "activity": []}),
    )
    .await;

    // hold the conversation's lock in a transaction of our own
    let mut holder = w.e.owner.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("fr-project:{conv}"))
        .execute(&mut *holder)
        .await
        .unwrap();

    let r = relay(&w);
    let mut budget = true;
    let blocked = tokio::time::timeout(
        Duration::from_secs(2),
        r.relay(w.f.tenant, w.f.job.job_id, "t-lock", &mut budget),
    )
    .await;
    assert!(
        blocked.is_err(),
        "the relay waits for the conversation's lock (it finished while the lock was held)"
    );
    let written = w
        .state(&w.f, &conv)
        .await
        .into_iter()
        .find(|s| s.0 == FR_PROJECT_ID)
        .and_then(|s| s.2);
    assert!(
        written.is_none(),
        "nothing is written while the lock is held: {written:?}"
    );

    holder.commit().await.unwrap();
    let mut budget = true;
    r.relay(w.f.tenant, w.f.job.job_id, "t-lock", &mut budget)
        .await
        .unwrap();
    let written = w
        .state(&w.f, &conv)
        .await
        .into_iter()
        .find(|s| s.0 == FR_PROJECT_ID)
        .and_then(|s| s.2);
    assert_eq!(written.as_deref(), Some(PROJECT));
}

/// A conveyor task names its project on the conveyor's own `_meta` channel: the id reaches the conversation's own
/// `conveyor_project_id` state (the key a job's `job_tool_policy` maps into `projects.close`), never the fr key, and
/// the project-started line and the run_step.changed event are posted once.
#[tokio::test]
async fn the_conveyor_project_id_is_state_for_close() {
    let (w, _seen, conv, case) = world("iam-b2-conveyor-progress").await;
    started(&w, &case, "t-conveyor").await;
    conveyor_meta(
        &w,
        "t-conveyor",
        json!({"project_id": "c-1", "run_id": "r-1", "status": "running", "events": [
            {"seq": 1, "type": "run.created", "payload": {"run_id": "r-1", "attempt": 1}},
            {"seq": 2, "type": "run.started", "payload": {"run_id": "r-1", "attempt": 1}},
            {"seq": 3, "type": "run_step.changed",
             "payload": {"position": 1, "attempt": 1, "status": "running"}}]}),
    )
    .await;

    let r = relay(&w);
    r.sweep().await.unwrap();
    assert_eq!(
        lines(&w, &conv).await,
        [task_relay::PROJECT_STARTED_TEXT, "Step 1: running"]
    );
    let state = w.state(&w.f, &conv).await;
    let value = |key: &str| state.iter().find(|s| s.0 == key).and_then(|s| s.2.clone());
    assert_eq!(value(CONVEYOR_PROJECT_ID).as_deref(), Some("c-1"));
    assert_eq!(value(CONVEYOR_PROJECT_TASK).as_deref(), Some("t-conveyor"));
    assert_eq!(
        value(FR_PROJECT_ID),
        None,
        "a conveyor project never lands on the fr key"
    );

    // due again with nothing new: the relay says nothing twice
    due_again(&w, "t-conveyor").await;
    r.sweep().await.unwrap();
    assert_eq!(lines(&w, &conv).await.len(), 2);
}

/// A finished conveyor task's structured result (`{run_id, project_id, status, result, error}`) is posted once,
/// naming the run's status and its result.
#[tokio::test]
async fn a_conveyor_terminal_result_is_relayed_once() {
    let (w, _seen, conv, case) = world("iam-b2-conveyor-result").await;
    started(&w, &case, "t-conveyor-done").await;
    sqlx::query(
        "UPDATE mcp_tasks SET status = 'completed', terminal_at = now(), result = $2 WHERE task_id = $1",
    )
    .bind("t-conveyor-done")
    .bind(json!({"outcome": "completed", "ok": true, "result": {
        "run_id": "r-1", "project_id": "c-1", "status": "succeeded", "result": "delivered"}}))
    .execute(&w.e.owner)
    .await
    .unwrap();

    let r = relay(&w);
    r.sweep().await.unwrap();
    let told = lines(&w, &conv).await;
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("succeeded"), "{told:?}");
    assert!(told[0].contains("delivered"), "{told:?}");
    let delivered: bool = sqlx::query_scalar(
        "SELECT delivered_at IS NOT NULL FROM mcp_tasks WHERE task_id = 't-conveyor-done'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(delivered);

    // a later relay of the same row is not delivered again
    let mut budget = true;
    r.relay(w.f.tenant, w.f.job.job_id, "t-conveyor-done", &mut budget)
        .await
        .unwrap();
    assert_eq!(lines(&w, &conv).await, told);
}

/// The due query reads the conveyor path when the task carries no fr block: a working task is due while its
/// `last_relayed_seq` is below its events' max seq, and not once it has caught up.
#[tokio::test]
async fn a_conveyor_task_is_due_for_its_activity() {
    let (w, _seen, _conv, case) = world("iam-b2-conveyor-due").await;
    started(&w, &case, "t-conveyor-due").await;
    conveyor_meta(
        &w,
        "t-conveyor-due",
        json!({"project_id": "c-1", "run_id": "r-1", "events": [
            {"seq": 1, "type": "run_step.changed",
             "payload": {"position": 1, "attempt": 1, "status": "running"}},
            {"seq": 2, "type": "run_step.changed",
             "payload": {"position": 2, "attempt": 1, "status": "running"}}]}),
    )
    .await;
    sqlx::query(
        "UPDATE mcp_tasks SET last_relayed_seq = 1, last_relayed_status = 'working' WHERE task_id = $1",
    )
    .bind("t-conveyor-due")
    .execute(&w.e.owner)
    .await
    .unwrap();
    assert!(is_due(&w, "t-conveyor-due").await, "seq 2 is new");

    sqlx::query("UPDATE mcp_tasks SET last_relayed_seq = 2 WHERE task_id = $1")
        .bind("t-conveyor-due")
        .execute(&w.e.owner)
        .await
        .unwrap();
    assert!(!is_due(&w, "t-conveyor-due").await, "nothing new to relay");
}

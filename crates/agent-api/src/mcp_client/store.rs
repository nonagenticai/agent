//! Durable MCP tasks (migration 0240), ported from care-bus `mcp_tasks.rs`.
//!
//! A long-running MCP tool comes back from the server as a task. [`record_started`] keeps the handle in `mcp_tasks`;
//! [`TaskPoller`] (run by the runtime daemon) claims due rows with `agent_mcp_task_claim`, polls `tasks/get`, retries
//! cancels and unsent staff answers, and stores the terminal outcome on the row before the server's `ttlMs` drops it.
//! No request and no tokio task is held per task. The conversation that started a task picks the outcome up with
//! [`undelivered_outcomes`] / [`mark_delivered`] (keyed on the case).
//!
//! Answers come from the route's authenticated staff caller, from the job's own model
//! (crate::runtime::task_answer, owner 2026-10-31), or from a customer's reply in the conversation
//! ([`answer`]); nothing here is reachable from an agent tool set.
//!
//! Escalation (owner 2026-10-31): the job's model answers by default; the task goes to the staff Tasks inbox
//! ([`escalate`]) when fr refuses an answer the agent may not give ([`Refusal::Escalate`]), when nobody answers within
//! [`TaskPoller::answer_timeout`], when the task fails, or when its conversation is handed over
//! ([`escalate_conversation`]). A refused answer is kept (`outcome`) and the question opens again for staff.
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use tokio::sync::watch;
use uuid::Uuid;

use crate::connectors::tasks::TaskClients;
use crate::error::{ApiError, ApiResult};
use crate::mcp_client::ToolError;
use crate::mcp_client::tasks::{
    StaffActor, TaskError, TaskHandle, TaskOutcome, TaskRef, TaskSnapshot, input_questions,
    outcome_of, poll_delay, validate_answer,
};
use crate::tenancy::begin_scoped;

/// After a transport failure, the next try.
const RETRY_AFTER: Duration = Duration::from_secs(15);

/// How the MCP server reports fr refusing an answer: the input opens again with this status message, then the code.
pub const REFUSED_PREFIX: &str = "fr refused the answer:";

/// What fr refusing an answer means here (fr#1685 codes, owner 2026-10-31).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A person must decide in fr: the task goes to the staff Tasks inbox. Also any code not listed below.
    Escalate,
    /// The question closed elsewhere in fr: nothing more to answer, nobody is told.
    Drop,
    /// A malformed request or a configuration problem (no agent member, wrong tenant): alerted, not escalated; the
    /// question is open again for staff, and the answer timeout still escalates it.
    Config,
}

/// The fr code of a refusal status message (`fr refused the answer: <code>`), if it is one.
pub fn refusal_code(status_message: Option<&str>) -> Option<String> {
    let rest = status_message?.trim().strip_prefix(REFUSED_PREFIX)?.trim();
    let code: String = rest
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    Some(if code.is_empty() {
        "error".to_owned()
    } else {
        code
    })
}

/// [`Refusal`] of an fr code.
pub fn classify(code: &str) -> Refusal {
    const DROP: [&str; 3] = ["approval_not_found", "already_decided", "approval_expired"];
    const CONFIG: [&str; 13] = [
        "actor_unresolved",
        "not_a_member",
        "approval_id_invalid",
        "approval_id_required",
        "unknown_command",
        "unknown_action",
        "invalid_input",
        "unknown_project_type",
        "title_required",
        "conveyor_not_configured",
        "project_types_unavailable",
        "tenant_mismatch",
        "tenant_ambiguous",
    ];
    if DROP.contains(&code) {
        Refusal::Drop
    } else if CONFIG.contains(&code) {
        Refusal::Config
    } else {
        Refusal::Escalate
    }
}

/// The handover priority (crate::conversations::handover's column vocabulary, 0211) a task signal's severity maps to:
/// `critical` is `urgent`, `high` is `high`, and `medium` / `low` / `info` (or a severity the wire does not know) is
/// `normal`. The relay escalates the two that need a person.
pub fn severity_priority(severity: &str) -> &'static str {
    match severity.trim().to_ascii_lowercase().as_str() {
        "critical" => "urgent",
        "high" => "high",
        _ => "normal",
    }
}

/// The rank [`escalate_upgrade`] compares: `critical` (2) is worse than `high` (1), and everything else (0) is a
/// severity that does not need a person. Trimmed and case-insensitive like [`severity_priority`].
pub fn severity_rank(severity: &str) -> i16 {
    match severity.trim().to_ascii_lowercase().as_str() {
        "critical" => 2,
        "high" => 1,
        _ => 0,
    }
}

/// Hand task `task_id` to staff for `reason` (once: an escalated task stays so). `notify` = the conversation is told
/// once by the relay (crate::runtime::task_relay); a handover or a failure already told it. Returns whether it was
/// escalated now.
pub async fn escalate(
    conn: &mut PgConnection,
    task_id: &str,
    reason: &str,
    notify: bool,
) -> ApiResult<bool> {
    let r = sqlx::query(
        "UPDATE mcp_tasks
            SET escalated_at = now(), escalation_reason = $2,
                escalation_notified_at = CASE WHEN $3 THEN NULL ELSE now() END, updated_at = now()
          WHERE task_id = $1 AND escalated_at IS NULL",
    )
    .bind(task_id)
    .bind(reason)
    .bind(notify)
    .execute(conn)
    .await?;
    let now = r.rows_affected() > 0;
    if now {
        tracing::info!(task_id, reason, "mcp tasks: escalated to staff");
    }
    Ok(now)
}

/// Escalate a task that is already escalated again, with a strictly worse `reason` (`rank` = [`severity_rank`] of the
/// new signal): the reason moves to the worse one and the notice claim is cleared so the relay tells the requester
/// about it (a repeat or a weaker signal changes nothing). Returns whether it upgraded.
///
/// ⛔ The `LIKE` prefixes below must stay in lock-step with the relay's `format!("fr:{severity}:{code}")`
/// (crate::runtime::task_relay `signals`): reading the rank an escalation already has is what "strictly worse" means.
pub async fn escalate_upgrade(
    conn: &mut PgConnection,
    task_id: &str,
    reason: &str,
    rank: i16,
) -> ApiResult<bool> {
    let r = sqlx::query(
        "UPDATE mcp_tasks SET escalated_at = now(), escalation_reason = $2, escalation_notified_at = NULL, updated_at = now()
         WHERE task_id = $1 AND escalated_at IS NOT NULL
           AND $3 > CASE WHEN escalation_reason LIKE 'fr:critical:%' THEN 2 WHEN escalation_reason LIKE 'fr:high:%' THEN 1 ELSE 0 END",
    )
    .bind(task_id)
    .bind(reason)
    .bind(rank)
    .execute(conn)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Hand every unfinished task of conversation `conversation_id` to staff (it was handed over: staff own it now).
pub async fn escalate_conversation(
    conn: &mut PgConnection,
    conversation_id: &str,
    reason: &str,
) -> ApiResult<u64> {
    let r = sqlx::query(
        "UPDATE mcp_tasks t
            SET escalated_at = now(), escalation_reason = $2, escalation_notified_at = now(), updated_at = now()
           FROM agent_case c
          WHERE c.case_id = t.case_id AND c.conversation_id = $1 AND t.terminal_at IS NULL
            AND t.escalated_at IS NULL",
    )
    .bind(conversation_id)
    .bind(reason)
    .execute(conn)
    .await?;
    Ok(r.rows_affected())
}

/// After a `tasks/get` of a task waiting for input: an answer fr refused is marked (and the task escalated, dropped or
/// alerted on per [`classify`]); a task nobody answered for `timeout` is escalated.
async fn after_input(
    conn: &mut PgConnection,
    task_id: &str,
    s: &TaskSnapshot,
    timeout: Duration,
) -> ApiResult<()> {
    if let Some(code) = refusal_code(s.status_message.as_deref()) {
        let open: Vec<String> = input_questions(&s.detail)
            .into_iter()
            .map(|q| q.key)
            .collect();
        let class = classify(&code);
        let outcome = if class == Refusal::Drop {
            "dropped"
        } else {
            "refused"
        };
        // only an answer that went out and whose question is open again was refused
        let r = sqlx::query(
            "UPDATE mcp_task_answers SET outcome = $3, refusal_code = $4, refused_at = now()
              WHERE task_id = $1 AND input_key = ANY($2) AND outcome IS NULL AND sent_at IS NOT NULL",
        )
        .bind(task_id)
        .bind(&open)
        .bind(outcome)
        .bind(&code)
        .execute(&mut *conn)
        .await?;
        if r.rows_affected() > 0 {
            match class {
                Refusal::Escalate => {
                    escalate(&mut *conn, task_id, &format!("refused:{code}"), true).await?;
                }
                Refusal::Drop => {
                    tracing::info!(
                        task_id,
                        code,
                        "mcp tasks: the question closed elsewhere in fr; dropped"
                    )
                }
                Refusal::Config => tracing::error!(
                    task_id,
                    code,
                    "mcp tasks: fr refused an answer as malformed or misconfigured; staff can answer it in the Tasks inbox"
                ),
            }
        }
    }
    sqlx::query(
        "UPDATE mcp_tasks
            SET escalated_at = now(), escalation_reason = 'no_answer', escalation_notified_at = NULL,
                updated_at = now()
          WHERE task_id = $1 AND status = 'input_required' AND escalated_at IS NULL
            AND input_since < now() - make_interval(secs => $2)",
    )
    .bind(task_id)
    .bind(timeout.as_secs_f64())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A transaction scoped to `tenant` and `job` (both GUCs).
pub async fn scoped(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
) -> ApiResult<Transaction<'static, Postgres>> {
    let mut tx = begin_scoped(db, tenant).await?;
    sqlx::query("SELECT set_config('app.current_job', $1, true)")
        .bind(job.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// A task about to be started, as its row stores it.
pub struct NewTask<'a> {
    pub tenant: Uuid,
    pub job: Uuid,
    /// The op_key whose call started the task.
    pub operation: &'a str,
    /// `OpRequest.idempotency_key`: links the task to the conversation / case waiting on it.
    pub idempotency_key: &'a str,
    pub case_id: Option<&'a str>,
    pub created_by: &'a str,
    /// The server that starts it: `platform` (the platform MCP server) or `connector` (a tenant's own system).
    pub origin: &'a str,
    /// The connector instance, when the origin is `connector` (NULL for the platform's tasks).
    pub instance: Option<&'a str>,
}

/// The row key of a started task. The server's id is unique only on its own server (SEP-2663), so a connector
/// task's key is namespaced `tenant:instance:server id`; a platform task has one server, and its key stays the
/// server's id. The server's own id is stored beside it (`server_task_id`).
fn stored_task_id(o: &NewTask<'_>, h: &TaskHandle) -> String {
    match o.origin {
        "connector" => format!("{}:{}:{}", o.tenant, o.instance.unwrap_or(""), h.task_id),
        _ => h.task_id.clone(),
    }
}

/// Keeps a freshly started task. Its first poll is after the server's `pollIntervalMs`.
pub async fn record_started(
    db: &PgPool,
    o: &NewTask<'_>,
    tool: &str,
    h: &TaskHandle,
) -> ApiResult<()> {
    let mut tx = scoped(db, o.tenant, o.job).await?;
    let status = if h.status == "input_required" {
        "input_required"
    } else {
        "working"
    };
    sqlx::query(
        "INSERT INTO mcp_tasks (tenant_id, job_id, task_id, server_task_id, operation, idempotency_key, case_id,
                                tool, status, status_message, poll_after, created_by, input_since, origin,
                                origin_instance)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now() + make_interval(secs => $11), $12,
                 CASE WHEN $9 = 'input_required' THEN now() END, $13, $14)",
    )
    .bind(o.tenant)
    .bind(o.job)
    .bind(stored_task_id(o, h))
    .bind(&h.task_id)
    .bind(o.operation)
    .bind(o.idempotency_key)
    .bind(o.case_id)
    .bind(tool)
    .bind(status)
    .bind(&h.status_message)
    .bind(poll_delay(h.poll_interval_ms).as_secs_f64())
    .bind(o.created_by)
    .bind(o.origin)
    .bind(o.instance)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Ends a row with `outcome`: the result is stored in the same statement that makes the row terminal. Delivery to the
/// conversation is separate (`delivered_at`).
async fn finish(
    conn: &mut PgConnection,
    task_id: &str,
    outcome: &TaskOutcome,
    detail: Option<&Value>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE mcp_tasks
            SET status = $2, result = $3, last_detail = coalesce($4, last_detail), terminal_at = now(),
                status_message = coalesce($4 ->> 'statusMessage', status_message), updated_at = now()
          WHERE task_id = $1 AND terminal_at IS NULL",
    )
    .bind(task_id)
    .bind(outcome.status())
    .bind(outcome.to_json())
    .bind(detail)
    .execute(conn)
    .await?;
    Ok(())
}

/// Stores one `tasks/get` answer: terminal -> [`finish`]; otherwise the status, its detail and the next poll time.
pub async fn apply_snapshot(
    conn: &mut PgConnection,
    task_id: &str,
    s: &TaskSnapshot,
) -> ApiResult<()> {
    if let Some(outcome) = outcome_of(s) {
        return finish(conn, task_id, &outcome, Some(&s.detail)).await;
    }
    let status = if s.status == "input_required" {
        "input_required"
    } else {
        "working"
    };
    sqlx::query(
        "UPDATE mcp_tasks
            SET status = $2, status_message = $3, last_detail = $4,
                poll_after = now() + make_interval(secs => $5), updated_at = now(),
                input_since = CASE WHEN $2 = 'input_required' THEN coalesce(input_since, now()) END
          WHERE task_id = $1 AND terminal_at IS NULL",
    )
    .bind(task_id)
    .bind(status)
    .bind(&s.status_message)
    .bind(&s.detail)
    .bind(poll_delay(s.poll_interval_ms).as_secs_f64())
    .execute(conn)
    .await?;
    Ok(())
}

async fn mark_lost(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
    message: &str,
) -> ApiResult<()> {
    let mut tx = scoped(db, tenant, job).await?;
    finish(
        &mut tx,
        task_id,
        &TaskOutcome::Lost(ToolError::fatal(message)),
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The poller. Each claim leases due rows for `lease_s`; a crashed poller's rows come back after that.
pub struct TaskPoller {
    pub db: PgPool,
    /// Where each task's row says it lives: the platform MCP server or its own connector system.
    pub clients: TaskClients,
    pub lease_s: i32,
    pub batch: i32,
    /// A task waiting this long for an answer goes to staff (`MCP_TASK_ANSWER_TIMEOUT_SECS`).
    pub answer_timeout: Duration,
}

#[derive(sqlx::FromRow)]
struct Due {
    tenant_id: Uuid,
    job_id: Uuid,
    task_id: String,
}

#[derive(sqlx::FromRow)]
struct Unsent {
    input_key: String,
    answered_by: String,
    response: Value,
}

impl TaskPoller {
    /// Polls until `shutdown`; sleeps `idle` when nothing is due.
    pub async fn work(self: Arc<Self>, mut shutdown: watch::Receiver<bool>, idle: Duration) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            let n = match self.tick().await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "mcp tasks: poll tick failed");
                    0
                }
            };
            if n == 0 {
                tokio::select! {
                    _ = tokio::time::sleep(idle) => {}
                    _ = shutdown.changed() => {}
                }
            }
        }
    }

    /// One claim and the work on every claimed row; returns how many were claimed.
    pub async fn tick(&self) -> ApiResult<usize> {
        let due: Vec<Due> =
            sqlx::query_as("SELECT tenant_id, job_id, task_id FROM agent_mcp_task_claim($1, $2)")
                .bind(self.lease_s)
                .bind(self.batch)
                .fetch_all(&self.db)
                .await?;
        for d in &due {
            if let Err(e) = self.poll_one(d).await {
                tracing::warn!(task_id = %d.task_id, error = %e, "mcp tasks: poll failed");
            }
        }
        Ok(due.len())
    }

    async fn poll_one(&self, d: &Due) -> ApiResult<()> {
        let (tenant, job, id) = (d.tenant_id, d.job_id, d.task_id.as_str());
        // Read what is owed to the server, then talk to it outside any transaction.
        let mut tx = scoped(&self.db, tenant, job).await?;
        let (cancel_owed, origin, origin_instance, server_task_id): (
            bool,
            String,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT cancel_requested AND NOT cancel_sent, origin, origin_instance, server_task_id
               FROM mcp_tasks WHERE task_id = $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        let unsent: Vec<Unsent> = sqlx::query_as(
            "SELECT input_key, answered_by, response FROM mcp_task_answers
              WHERE task_id = $1 AND sent_at IS NULL AND outcome IS NULL",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let task = TaskRef {
            tenant,
            job,
            task_id: id,
            server_task_id: &server_task_id,
            origin: &origin,
            instance: origin_instance.as_deref(),
        };

        if cancel_owed {
            match self.clients.cancel(&task).await {
                Ok(()) => set_cancel_sent(&self.db, tenant, job, id).await?,
                Err(TaskError::Lost(m)) => return mark_lost(&self.db, tenant, job, id, &m).await,
                Err(TaskError::Tool(e)) => {
                    tracing::warn!(task_id = id, error = %e.message, "mcp tasks: cancel not sent; retried")
                }
            }
        }
        for a in unsent {
            let actor = StaffActor {
                tenant,
                staff_user: a.answered_by,
            };
            match self
                .clients
                .answer(&task, &actor, &a.input_key, a.response)
                .await
            {
                Ok(()) => set_answer_sent(&self.db, tenant, job, id, &a.input_key).await?,
                Err(TaskError::Lost(m)) => return mark_lost(&self.db, tenant, job, id, &m).await,
                Err(TaskError::Tool(e)) => {
                    tracing::warn!(task_id = id, error = %e.message, "mcp tasks: answer not sent; retried")
                }
            }
        }

        match self.clients.get(&task).await {
            Ok(s) => {
                let mut tx = scoped(&self.db, tenant, job).await?;
                apply_snapshot(&mut tx, id, &s).await?;
                let outcome = outcome_of(&s);
                if outcome.is_none() && s.status == "input_required" {
                    after_input(&mut tx, id, &s, self.answer_timeout).await?;
                }
                // a failed task needs a person; the conversation is told by the outcome itself
                if outcome.as_ref().is_some_and(|o| o.status() == "failed") {
                    escalate(&mut tx, id, "failed", false).await?;
                }
                tx.commit().await?;
                if let Some(o) = outcome {
                    tracing::info!(
                        task_id = id,
                        outcome = o.status(),
                        "mcp tasks: task finished"
                    );
                }
                Ok(())
            }
            Err(TaskError::Lost(m)) => mark_lost(&self.db, tenant, job, id, &m).await,
            Err(TaskError::Tool(e)) => {
                tracing::warn!(task_id = id, error = %e.message, "mcp tasks: tasks/get failed; retried");
                let mut tx = scoped(&self.db, tenant, job).await?;
                sqlx::query(
                    "UPDATE mcp_tasks SET poll_after = now() + make_interval(secs => $2), updated_at = now()
                      WHERE task_id = $1 AND terminal_at IS NULL",
                )
                .bind(id)
                .bind(RETRY_AFTER.as_secs_f64())
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }
}

async fn set_cancel_sent(db: &PgPool, tenant: Uuid, job: Uuid, id: &str) -> ApiResult<()> {
    let mut tx = scoped(db, tenant, job).await?;
    sqlx::query("UPDATE mcp_tasks SET cancel_sent = true, poll_after = now(), updated_at = now() WHERE task_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn set_answer_sent(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    id: &str,
    key: &str,
) -> ApiResult<()> {
    let mut tx = scoped(db, tenant, job).await?;
    sqlx::query(
        "UPDATE mcp_task_answers SET sent_at = now() WHERE task_id = $1 AND input_key = $2 AND outcome IS NULL",
    )
    .bind(id)
    .bind(key)
    .execute(&mut *tx)
    .await?;
    // The decision moves the task on: poll it soon.
    sqlx::query("UPDATE mcp_tasks SET poll_after = now(), updated_at = now() WHERE task_id = $1 AND terminal_at IS NULL")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The questions of a stored detail that nobody here answered yet.
pub fn open_questions(detail: Option<&Value>, answered: &[String]) -> Vec<Value> {
    detail
        .map(input_questions)
        .unwrap_or_default()
        .into_iter()
        .filter(|q| !answered.contains(&q.key))
        .map(
            |q| json!({"key": q.key, "message": q.message, "requested_schema": q.requested_schema}),
        )
        .collect()
}

/// A staff answer as posted: the question's key, `accept` / `decline` / `cancel`, and the content for `accept`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerInput {
    pub key: String,
    pub action: String,
    #[serde(default)]
    pub content: Option<Value>,
}

/// What happened to a staff answer.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct AnswerResult {
    /// The server acknowledged it now; `false` = kept and retried by the poller.
    pub sent: bool,
    /// This key was already answered here: nothing was sent again.
    pub already_answered: bool,
}

fn not_found(task_id: &str) -> ApiError {
    ApiError::Missing("task_not_found", format!("task {task_id}"))
}

/// The row an answer needs under its lock: its status, the last `tasks/get` detail, and where the task lives.
#[derive(sqlx::FromRow)]
struct AnswerRow {
    status: String,
    last_detail: Option<Value>,
    origin: String,
    origin_instance: Option<String>,
    server_task_id: String,
}

/// One answer: a staff member's, or the job's for a customer's reply (`source` = the conversation messages it came
/// from, kept for audit: fr records only the job). The task must be waiting on `key` (else 409 `not_waiting` /
/// `answered_elsewhere`), the answer must meet the question's schema (else 422 `invalid_answer`). The answer is stored
/// first (a double submit is a no-op; an answer fr refused does not count, so the question can be answered again),
/// then sent; a failed send is retried by the poller.
pub async fn answer(
    db: &PgPool,
    clients: &TaskClients,
    actor: &StaffActor,
    job: Uuid,
    task_id: &str,
    input: &AnswerInput,
    source: &[String],
) -> ApiResult<AnswerResult> {
    let (key, action, content) = (
        input.key.as_str(),
        input.action.as_str(),
        input.content.as_ref(),
    );
    let mut tx = scoped(db, actor.tenant, job).await?;
    let row: Option<AnswerRow> = sqlx::query_as(
        "SELECT status, last_detail, origin, origin_instance, server_task_id FROM mcp_tasks WHERE task_id = $1 FOR UPDATE",
    )
    .bind(task_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(AnswerRow {
        status,
        last_detail: detail,
        origin,
        origin_instance,
        server_task_id,
    }) = row
    else {
        return Err(not_found(task_id));
    };
    let done: Option<(String,)> = sqlx::query_as(
        "SELECT answered_by FROM mcp_task_answers
          WHERE task_id = $1 AND input_key = $2 AND outcome IS DISTINCT FROM 'refused'",
    )
    .bind(task_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    if done.is_some() {
        return Ok(AnswerResult {
            sent: false,
            already_answered: true,
        });
    }
    if status != "input_required" {
        return Err(ApiError::Conflict(
            "not_waiting",
            format!("This task is {status}, not waiting for input."),
        ));
    }
    let questions = detail.as_ref().map(input_questions).unwrap_or_default();
    let Some(q) = questions.iter().find(|q| q.key == key) else {
        return Err(ApiError::Conflict(
            "answered_elsewhere",
            "This question is no longer open: it was answered on another channel.".into(),
        ));
    };
    let response = validate_answer(&q.requested_schema, action, content)
        .map_err(|e| ApiError::Unprocessable("invalid_answer", e))?;
    sqlx::query(
        "INSERT INTO mcp_task_answers (tenant_id, job_id, task_id, input_key, answered_by, response, source_messages,
                                       attempt)
         VALUES ($1, $2, $3, $4, $5, $6, $7,
                 (SELECT coalesce(max(attempt), 0) + 1 FROM mcp_task_answers WHERE task_id = $3 AND input_key = $4))",
    )
    .bind(actor.tenant)
    .bind(job)
    .bind(task_id)
    .bind(key)
    .bind(&actor.staff_user)
    .bind(&response)
    .bind(source)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let task = TaskRef {
        tenant: actor.tenant,
        job,
        task_id,
        server_task_id: &server_task_id,
        origin: &origin,
        instance: origin_instance.as_deref(),
    };

    match clients.answer(&task, actor, key, response).await {
        Ok(()) => {
            set_answer_sent(db, actor.tenant, job, task_id, key).await?;
            Ok(AnswerResult {
                sent: true,
                already_answered: false,
            })
        }
        Err(TaskError::Lost(m)) => {
            mark_lost(db, actor.tenant, job, task_id, &m).await?;
            Err(ApiError::Conflict(
                "task_lost",
                "The server no longer knows this task.".into(),
            ))
        }
        Err(TaskError::Tool(e)) => {
            tracing::warn!(task_id, error = %e.message, "mcp tasks: answer kept, send retried by the poller");
            Ok(AnswerResult {
                sent: false,
                already_answered: false,
            })
        }
    }
}

/// Asks the server to cancel. The row stays non-terminal until `tasks/get` says otherwise; a failed send is retried
/// by the poller. A finished task is 409 `task_finished`. Returns whether the server acknowledged now.
pub async fn cancel(
    db: &PgPool,
    clients: &TaskClients,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
) -> ApiResult<bool> {
    let mut tx = scoped(db, tenant, job).await?;
    let row: Option<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT status, origin, origin_instance, server_task_id FROM mcp_tasks WHERE task_id = $1 FOR UPDATE",
    )
    .bind(task_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, origin, origin_instance, server_task_id)) = row else {
        return Err(not_found(task_id));
    };
    if matches!(
        status.as_str(),
        "completed" | "cancelled" | "failed" | "lost"
    ) {
        return Err(ApiError::Conflict(
            "task_finished",
            format!("This task is already {status}."),
        ));
    }
    sqlx::query(
        "UPDATE mcp_tasks SET cancel_requested = true, updated_at = now() WHERE task_id = $1",
    )
    .bind(task_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let task = TaskRef {
        tenant,
        job,
        task_id,
        server_task_id: &server_task_id,
        origin: &origin,
        instance: origin_instance.as_deref(),
    };
    match clients.cancel(&task).await {
        Ok(()) => {
            set_cancel_sent(db, tenant, job, task_id).await?;
            Ok(true)
        }
        Err(TaskError::Lost(m)) => {
            mark_lost(db, tenant, job, task_id, &m).await?;
            Ok(false)
        }
        Err(TaskError::Tool(e)) => {
            tracing::warn!(task_id, error = %e.message, "mcp tasks: cancel kept, send retried by the poller");
            Ok(false)
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    /// The row's key: namespaced for a connector task (`tenant:instance:server id`), the server id for a platform
    /// task.
    task_id: String,
    /// The server's own task id.
    server_task_id: String,
    operation: String,
    tool: String,
    status: String,
    status_message: Option<String>,
    case_id: Option<String>,
    last_detail: Option<Value>,
    result: Option<Value>,
    cancel_requested: bool,
    created_at: DateTime<Utc>,
    terminal_at: Option<DateTime<Utc>>,
    answered: Vec<String>,
    escalated_at: Option<DateTime<Utc>>,
    escalation_reason: Option<String>,
}

/// One task as the staff inbox shows it.
#[derive(Debug, Serialize)]
pub struct TaskView {
    pub task_id: String,
    /// The server's own task id, beside the row's (possibly namespaced) key.
    pub server_task_id: String,
    pub operation: String,
    pub tool: String,
    pub status: String,
    pub status_message: Option<String>,
    pub case_id: Option<String>,
    /// Open questions nobody here answered yet: `{key, message, requested_schema}`.
    pub questions: Vec<Value>,
    pub result: Option<Value>,
    pub cancel_requested: bool,
    pub created_at: DateTime<Utc>,
    pub terminal_at: Option<DateTime<Utc>>,
    /// When staff were asked to take the task over, and why (`refused:<fr code>`, `no_answer`, `failed`, `handover`).
    pub escalated_at: Option<DateTime<Utc>>,
    pub escalation_reason: Option<String>,
}

impl From<Row> for TaskView {
    fn from(r: Row) -> Self {
        let questions = if r.status == "input_required" {
            open_questions(r.last_detail.as_ref(), &r.answered)
        } else {
            Vec::new()
        };
        Self {
            task_id: r.task_id,
            server_task_id: r.server_task_id,
            operation: r.operation,
            tool: r.tool,
            status: r.status,
            status_message: r.status_message,
            case_id: r.case_id,
            questions,
            result: r.result,
            cancel_requested: r.cancel_requested,
            created_at: r.created_at,
            terminal_at: r.terminal_at,
            escalated_at: r.escalated_at,
            escalation_reason: r.escalation_reason,
        }
    }
}

const VIEW_COLS: &str =
    "t.task_id, t.server_task_id, t.operation, t.tool, t.status, t.status_message, t.case_id,
        t.last_detail, t.result, t.cancel_requested, t.created_at, t.terminal_at, t.escalated_at,
        t.escalation_reason,
        ARRAY(SELECT a.input_key FROM mcp_task_answers a
               WHERE a.task_id = t.task_id AND a.outcome IS DISTINCT FROM 'refused') AS answered";

/// The job's tasks, newest first; `open_only` = working / input_required only.
pub async fn list(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    open_only: bool,
    limit: i64,
) -> ApiResult<Vec<TaskView>> {
    let mut tx = scoped(db, tenant, job).await?;
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {VIEW_COLS} FROM mcp_tasks t
          WHERE (NOT $1 OR t.status IN ('working', 'input_required'))
          ORDER BY t.created_at DESC LIMIT $2"
    )))
    .bind(open_only)
    .bind(limit.clamp(1, 500))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(TaskView::from).collect())
}

/// One task of the job; 404 `task_not_found` when it is not this job's.
pub async fn one(db: &PgPool, tenant: Uuid, job: Uuid, task_id: &str) -> ApiResult<TaskView> {
    let mut tx = scoped(db, tenant, job).await?;
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {VIEW_COLS} FROM mcp_tasks t WHERE t.task_id = $1"
    )))
    .bind(task_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    row.map(TaskView::from).ok_or_else(|| not_found(task_id))
}

/// A terminal task whose outcome the conversation has not been given yet.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Undelivered {
    pub task_id: String,
    pub operation: String,
    pub idempotency_key: String,
    pub tool: String,
    pub status: String,
    /// `TaskOutcome::to_json`.
    pub result: Option<Value>,
}

/// Terminal, not yet delivered tasks of `case_id` in the job, oldest first.
pub async fn undelivered_outcomes(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    case_id: &str,
) -> ApiResult<Vec<Undelivered>> {
    let mut tx = scoped(db, tenant, job).await?;
    let rows = sqlx::query_as(
        "SELECT task_id, operation, idempotency_key, tool, status, result FROM mcp_tasks
          WHERE case_id = $1 AND terminal_at IS NOT NULL AND delivered_at IS NULL
          ORDER BY terminal_at",
    )
    .bind(case_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Marks a task's outcome as handed to the conversation (idempotent); true when this call did it.
pub async fn mark_delivered(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
) -> ApiResult<bool> {
    let mut tx = scoped(db, tenant, job).await?;
    let n = sqlx::query(
        "UPDATE mcp_tasks SET delivered_at = now(), updated_at = now()
          WHERE task_id = $1 AND terminal_at IS NOT NULL AND delivered_at IS NULL",
    )
    .bind(task_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(n == 1)
}

/// Releases the once-only delivery claim after a failed post so the next relay sweep retries it.
pub async fn unmark_delivered(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    task_id: &str,
) -> ApiResult<()> {
    let mut tx = scoped(db, tenant, job).await?;
    sqlx::query(
        "UPDATE mcp_tasks SET delivered_at = NULL, updated_at = now()
          WHERE task_id = $1 AND delivered_at IS NOT NULL",
    )
    .bind(task_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_questions_skip_answered_keys() {
        let d = json!({"inputRequests": {
            "t:1": {"method": "elicitation/create", "params": {"message": "A?", "requestedSchema": {}}},
            "t:2": {"method": "elicitation/create", "params": {"message": "B?", "requestedSchema": {}}}}});
        let q = open_questions(Some(&d), &["t:1".to_string()]);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0]["key"], "t:2");
        assert!(open_questions(None, &[]).is_empty());
    }

    #[test]
    fn a_view_shows_questions_only_while_input_is_required() {
        let d = json!({"inputRequests": {
            "t:1": {"method": "elicitation/create", "params": {"message": "A?", "requestedSchema": {}}}}});
        let row = |status: &str| Row {
            task_id: "t".into(),
            server_task_id: "t".into(),
            operation: "op".into(),
            tool: "tool".into(),
            status: status.into(),
            status_message: None,
            case_id: None,
            last_detail: Some(d.clone()),
            result: None,
            cancel_requested: false,
            created_at: Utc::now(),
            terminal_at: None,
            answered: vec![],
            escalated_at: None,
            escalation_reason: None,
        };
        assert_eq!(TaskView::from(row("input_required")).questions.len(), 1);
        assert!(TaskView::from(row("working")).questions.is_empty());
    }

    #[test]
    fn a_signals_severity_maps_to_a_handover_priority() {
        assert_eq!(severity_priority("critical"), "urgent");
        assert_eq!(severity_priority("High"), "high");
        assert_eq!(severity_priority("medium"), "normal");
        assert_eq!(severity_priority("low"), "normal");
        assert_eq!(severity_priority("info"), "normal");
        assert_eq!(severity_priority("something_new"), "normal");
    }

    #[test]
    fn a_worse_severity_ranks_higher() {
        assert_eq!(severity_rank("critical"), 2);
        assert_eq!(severity_rank(" CRITICAL "), 2);
        assert_eq!(severity_rank("high"), 1);
        assert_eq!(severity_rank("High"), 1);
        for s in ["medium", "low", "info", "", "something_new"] {
            assert_eq!(severity_rank(s), 0, "{s}");
        }
    }

    #[test]
    fn a_refusal_names_its_fr_code_and_what_it_means_here() {
        assert_eq!(
            refusal_code(Some("fr refused the answer: agent_decision_not_allowed")).as_deref(),
            Some("agent_decision_not_allowed")
        );
        assert_eq!(
            refusal_code(Some("fr refused the answer: ")).as_deref(),
            Some("error")
        );
        assert_eq!(refusal_code(Some("waiting for approval")), None);
        assert_eq!(refusal_code(None), None);
        for c in [
            "agent_decision_not_allowed",
            "no_decide_rights",
            "decide_in_fr",
            "approver_is_requester",
            "forbidden",
            "error",
            "fr_error",
            "something_new",
        ] {
            assert_eq!(classify(c), Refusal::Escalate, "{c}");
        }
        for c in ["approval_not_found", "already_decided", "approval_expired"] {
            assert_eq!(classify(c), Refusal::Drop, "{c}");
        }
        for c in [
            "actor_unresolved",
            "not_a_member",
            "unknown_command",
            "tenant_mismatch",
        ] {
            assert_eq!(classify(c), Refusal::Config, "{c}");
        }
    }
}
